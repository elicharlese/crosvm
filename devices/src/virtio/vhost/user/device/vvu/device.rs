// Copyright 2021 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implement a struct that works as a `vmm_vhost`'s backend.

use std::cmp::Ordering;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::mem;
use std::os::unix::prelude::RawFd;
use std::sync::mpsc::channel;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use base::error;
use base::info;
use base::Event;
use base::RawDescriptor;
use cros_async::EventAsync;
use cros_async::Executor;
use futures::pin_mut;
use futures::select;
use futures::FutureExt;
use sync::Mutex;
use vmm_vhost::connection::vfio::Device as VfioDeviceTrait;
use vmm_vhost::connection::vfio::Endpoint as VfioEndpoint;
use vmm_vhost::connection::vfio::RecvIntoBufsError;
use vmm_vhost::connection::Endpoint;
use vmm_vhost::message::*;

use crate::virtio::vhost::user::device::vvu::pci::QueueNotifier;
use crate::virtio::vhost::user::device::vvu::pci::VvuPciDevice;
use crate::virtio::vhost::user::device::vvu::queue::UserQueue;
use crate::virtio::vhost::vhost_header_from_bytes;
use crate::virtio::vhost::HEADER_LEN;

// Helper class for forwarding messages from the virtqueue thread to the main worker thread.
struct VfioSender {
    sender: Sender<Vec<u8>>,
    evt: Event,
}

impl VfioSender {
    fn new(sender: Sender<Vec<u8>>, evt: Event) -> Self {
        Self { sender, evt }
    }

    fn send(&self, buf: Vec<u8>) -> Result<()> {
        self.sender.send(buf)?;
        // Increment the event counter as we sent one buffer.
        self.evt.write(1).context("failed to signal event")
    }
}

struct VfioReceiver {
    receiver: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    offset: usize,
    evt: Event,
}

// Utility class for converting discrete vhost user messages received by a
// VfioSender into a byte stream.
impl VfioReceiver {
    fn new(receiver: Receiver<Vec<u8>>, evt: Event) -> Self {
        Self {
            receiver,
            buf: Vec::new(),
            offset: 0,
            evt,
        }
    }

    // Reads the vhost user message into a byte stream. After each discrete message has
    // been consumed, returns the message for post-processing.
    fn recv_into_buf(
        &mut self,
        out: &mut IoSliceMut,
    ) -> Result<(usize, Option<Vec<u8>>), RecvIntoBufsError> {
        let len = out.len();

        if self.buf.is_empty() {
            let data = self
                .receiver
                .recv()
                .context("failed to receive data")
                .map_err(RecvIntoBufsError::Fatal)?;

            if data.len() == 0 {
                // TODO(b/216407443): We should change `self.state` and exit gracefully.
                info!("VVU connection is closed");
                return Err(RecvIntoBufsError::Disconnect);
            }

            self.buf = data;
            self.offset = 0;
            // Decrement the event counter as we received one buffer.
            self.evt
                .read()
                .and_then(|c| self.evt.write(c - 1))
                .context("failed to decrease event counter")
                .map_err(RecvIntoBufsError::Fatal)?;
        }

        if self.offset + len > self.buf.len() {
            // VVU rxq runs at message granularity. If there's not enough bytes to fill
            // |out|, then that means we're being asked to merge bytes from multiple messages
            // into a single buffer. That almost certainly indicates a message framing error
            // higher up the stack, so reject the request.
            return Err(RecvIntoBufsError::Fatal(anyhow!(
                "recv underflow {} {} {}",
                self.offset,
                len,
                self.buf.len()
            )));
        }
        out.clone_from_slice(&self.buf[self.offset..(self.offset + len)]);

        self.offset += len;
        let ret_vec = if self.offset == self.buf.len() {
            Some(std::mem::take(&mut self.buf))
        } else {
            None
        };

        Ok((len, ret_vec))
    }

    fn recv_into_bufs(&mut self, bufs: &mut [IoSliceMut]) -> Result<usize, RecvIntoBufsError> {
        let mut size = 0;
        for buf in bufs {
            let (len, _) = self.recv_into_buf(buf)?;
            size += len;
        }

        Ok(size)
    }
}

// Data queued to send on an endpoint.
#[derive(Default)]
struct EndpointTxBuffer {
    bytes: Vec<u8>,
}

// Utility class for writing an input vhost-user byte stream to the vvu
// tx virtqueue as discrete vhost-user messages.
struct Queue {
    txq: UserQueue,
    txq_notifier: QueueNotifier,
}

impl Queue {
    fn send_bufs(
        &mut self,
        iovs: &[IoSlice],
        fds: Option<&[RawDescriptor]>,
        tx_state: &mut EndpointTxBuffer,
    ) -> Result<usize> {
        if fds.is_some() {
            bail!("cannot send FDs");
        }

        let mut size = 0;
        for iov in iovs {
            let mut vec = iov.to_vec();
            size += iov.len();
            tx_state.bytes.append(&mut vec);
        }

        if let Some(hdr) = vhost_header_from_bytes::<MasterReq>(&tx_state.bytes) {
            let bytes_needed = hdr.get_size() as usize + HEADER_LEN;
            match bytes_needed.cmp(&tx_state.bytes.len()) {
                Ordering::Greater => (),
                Ordering::Equal => {
                    let msg = mem::take(&mut tx_state.bytes);
                    self.txq.write(&msg).context("Failed to send data")?;
                }
                Ordering::Less => bail!("sent bytes larger than message size"),
            }
        }
        self.txq_notifier.notify();

        Ok(size)
    }
}

async fn process_rxq(
    evt: EventAsync,
    mut rxq: UserQueue,
    rxq_notifier: QueueNotifier,
    frontend_sender: VfioSender,
    backend_sender: VfioSender,
) -> Result<()> {
    loop {
        if let Err(e) = evt.next_val().await {
            error!("Failed to read the next queue event: {}", e);
            continue;
        }

        while let Some(slice) = rxq.read_data()? {
            if slice.size() < HEADER_LEN {
                bail!("rxq message too short: {}", slice.size());
            }

            let mut buf = vec![0_u8; slice.size()];
            slice.copy_to(&mut buf);

            // The inbound message may be a SlaveReq message. However, the values
            // of all SlaveReq enum values can be safely interpreted as MasterReq
            // enum values.
            let hdr =
                vhost_header_from_bytes::<MasterReq>(&buf).context("rxq message too short")?;
            if HEADER_LEN + hdr.get_size() as usize != slice.size() {
                bail!(
                    "rxq message size mismatch: {} vs {}",
                    slice.size(),
                    hdr.get_size()
                );
            }

            if hdr.is_reply() {
                &backend_sender
            } else {
                &frontend_sender
            }
            .send(buf)
            .context("send failed")?;
        }
        rxq_notifier.notify();
    }
}

async fn process_txq(evt: EventAsync, txq: Arc<Mutex<Queue>>) -> Result<()> {
    loop {
        if let Err(e) = evt.next_val().await {
            error!("Failed to read the next queue event: {}", e);
            continue;
        }

        txq.lock().txq.ack_used()?;
    }
}

fn run_worker(
    ex: Executor,
    rx_queue: UserQueue,
    rx_irq: Event,
    rx_notifier: QueueNotifier,
    frontend_sender: VfioSender,
    backend_sender: VfioSender,
    tx_queue: Arc<Mutex<Queue>>,
    tx_irq: Event,
) -> Result<()> {
    let rx_irq = EventAsync::new(rx_irq, &ex).context("failed to create async event")?;
    let rxq = process_rxq(
        rx_irq,
        rx_queue,
        rx_notifier,
        frontend_sender,
        backend_sender,
    );
    pin_mut!(rxq);

    let tx_irq = EventAsync::new(tx_irq, &ex).context("failed to create async event")?;
    let txq = process_txq(tx_irq, Arc::clone(&tx_queue));
    pin_mut!(txq);

    let done = async {
        select! {
            res = rxq.fuse() => res.context("failed to handle rxq"),
            res = txq.fuse() => res.context("failed to handle txq"),
        }
    };

    match ex.run_until(done) {
        Ok(_) => Ok(()),
        Err(e) => {
            bail!("failed to process virtio-vhost-user queues: {}", e);
        }
    }
}

enum DeviceState {
    Initialized {
        // TODO(keiichiw): Update `VfioDeviceTrait::start()` to take `VvuPciDevice` so that we can
        // drop this field.
        device: VvuPciDevice,
    },
    Running {
        rxq_receiver: VfioReceiver,
        tx_state: EndpointTxBuffer,

        txq: Arc<Mutex<Queue>>,
    },
}

pub struct VvuDevice {
    state: DeviceState,
    frontend_rxq_evt: Event,

    backend_channel: Option<VfioEndpoint<SlaveReq, BackendChannel>>,
}

impl VvuDevice {
    pub fn new(device: VvuPciDevice) -> Self {
        Self {
            state: DeviceState::Initialized { device },
            frontend_rxq_evt: Event::new().expect("failed to create VvuDevice's rxq_evt"),
            backend_channel: None,
        }
    }
}

impl VfioDeviceTrait for VvuDevice {
    fn event(&self) -> &Event {
        &self.frontend_rxq_evt
    }

    fn start(&mut self) -> Result<()> {
        let device = match &mut self.state {
            DeviceState::Initialized { device } => device,
            DeviceState::Running { .. } => {
                bail!("VvuDevice has already started");
            }
        };
        let ex = Executor::new().expect("Failed to create an executor");

        let mut irqs = mem::take(&mut device.irqs);
        let mut queues = mem::take(&mut device.queues);
        let mut queue_notifiers = mem::take(&mut device.queue_notifiers);

        let rxq = queues.remove(0);
        let rxq_irq = irqs.remove(0);
        let rxq_notifier = queue_notifiers.remove(0);
        // TODO: Can we use async channel instead so we don't need `rxq_evt`?
        let (rxq_sender, rxq_receiver) = channel();
        let rxq_evt = self.frontend_rxq_evt.try_clone().expect("rxq_evt clone");

        let txq = Arc::new(Mutex::new(Queue {
            txq: queues.remove(0),
            txq_notifier: queue_notifiers.remove(0),
        }));
        let txq_cloned = Arc::clone(&txq);
        let txq_irq = irqs.remove(0);

        let (backend_rxq_sender, backend_rxq_receiver) = channel();
        let backend_rxq_evt = Event::new().expect("failed to create VvuDevice's rxq_evt");
        let backend_rxq_evt2 = backend_rxq_evt.try_clone().expect("rxq_evt clone");
        self.backend_channel = Some(VfioEndpoint::from(BackendChannel {
            receiver: VfioReceiver::new(backend_rxq_receiver, backend_rxq_evt),
            queue: txq.clone(),
            tx_state: EndpointTxBuffer::default(),
        }));

        let old_state = std::mem::replace(
            &mut self.state,
            DeviceState::Running {
                rxq_receiver: VfioReceiver::new(
                    rxq_receiver,
                    self.frontend_rxq_evt
                        .try_clone()
                        .expect("frontend_rxq_evt clone"),
                ),
                tx_state: EndpointTxBuffer::default(),
                txq,
            },
        );

        let device = match old_state {
            DeviceState::Initialized { device } => device,
            _ => unreachable!(),
        };

        let frontend_sender = VfioSender::new(rxq_sender, rxq_evt);
        let backend_sender = VfioSender::new(backend_rxq_sender, backend_rxq_evt2);
        thread::Builder::new()
            .name("virtio-vhost-user driver".to_string())
            .spawn(move || {
                device.start().expect("failed to start device");
                if let Err(e) = run_worker(
                    ex,
                    rxq,
                    rxq_irq,
                    rxq_notifier,
                    frontend_sender,
                    backend_sender,
                    txq_cloned,
                    txq_irq,
                ) {
                    error!("worker thread exited with error: {}", e);
                }
            })?;

        Ok(())
    }

    fn send_bufs(&mut self, iovs: &[IoSlice], fds: Option<&[RawDescriptor]>) -> Result<usize> {
        match &mut self.state {
            DeviceState::Initialized { .. } => {
                bail!("VvuDevice hasn't started yet");
            }
            DeviceState::Running { txq, tx_state, .. } => {
                let mut queue = txq.lock();
                queue.send_bufs(iovs, fds, tx_state)
            }
        }
    }

    fn recv_into_bufs(&mut self, bufs: &mut [IoSliceMut]) -> Result<usize, RecvIntoBufsError> {
        match &mut self.state {
            DeviceState::Initialized { .. } => Err(RecvIntoBufsError::Fatal(anyhow!(
                "VvuDevice hasn't started yet"
            ))),
            DeviceState::Running { rxq_receiver, .. } => rxq_receiver.recv_into_bufs(bufs),
        }
    }

    fn create_slave_request_endpoint(&mut self) -> Result<Box<dyn Endpoint<SlaveReq>>> {
        self.backend_channel
            .take()
            .map_or(Err(anyhow!("missing backend endpoint")), |c| {
                Ok(Box::new(c))
            })
    }
}

// Struct which implements the Endpoint for backend messages.
struct BackendChannel {
    receiver: VfioReceiver,
    queue: Arc<Mutex<Queue>>,
    tx_state: EndpointTxBuffer,
}

impl VfioDeviceTrait for BackendChannel {
    fn event(&self) -> &Event {
        &self.receiver.evt
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn send_bufs(&mut self, iovs: &[IoSlice], fds: Option<&[RawFd]>) -> Result<usize> {
        self.queue.lock().send_bufs(iovs, fds, &mut self.tx_state)
    }

    fn recv_into_bufs(&mut self, bufs: &mut [IoSliceMut]) -> Result<usize, RecvIntoBufsError> {
        self.receiver.recv_into_bufs(bufs)
    }

    fn create_slave_request_endpoint(&mut self) -> Result<Box<dyn Endpoint<SlaveReq>>> {
        Err(anyhow!(
            "can't construct backend endpoint from backend endpoint"
        ))
    }
}
