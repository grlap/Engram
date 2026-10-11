use super::{Observer, Pending};
use crate::phase_trace::ShutdownStage;
use rmcp::{
    RoleServer,
    model::{ClientNotification, ClientRequest, JsonRpcMessage},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use std::{future::Future, sync::Arc, time::Instant};

pub struct DiagnosticTransport<T> {
    inner: T,
    observer: Arc<Observer>,
}
impl<T> DiagnosticTransport<T> {
    pub const fn new(inner: T, observer: Arc<Observer>) -> Self {
        Self { inner, observer }
    }
}

struct Sending {
    observer: Arc<Observer>,
    item: Option<Pending>,
    fallback: Option<super::Correlation>,
    dispatch: Instant,
}
impl Drop for Sending {
    fn drop(&mut self) {
        if let Some(item) = self.item.take() {
            self.observer
                .sink
                .emit(item.event(Instant::now(), super::Fault::incomplete()));
        } else if let Some(correlation) = self.fallback.take() {
            self.observer.sink.emit(super::Event::failure(
                "unknown",
                correlation,
                self.dispatch.elapsed(),
                super::Fault::incomplete(),
            ));
        }
    }
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for DiagnosticTransport<T> {
    type Error = T::Error;
    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let item = match &message {
            JsonRpcMessage::Response(response) => self.observer.dispatching(&response.id, false),
            JsonRpcMessage::Error(error) => {
                let item = error
                    .id
                    .as_ref()
                    .and_then(|id| self.observer.dispatching(id, true));
                if item.is_none() {
                    self.observer.sink.emit(super::Event::failure(
                        "unknown",
                        error
                            .id
                            .as_ref()
                            .map_or_else(super::Correlation::default, |id| {
                                self.observer.untracked(id)
                            }),
                        std::time::Duration::ZERO,
                        super::Fault::rpc_error(),
                    ));
                }
                item
            }
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
        };
        let mut sending = Sending {
            observer: Arc::clone(&self.observer),
            item,
            fallback: if item.is_none() {
                match &message {
                    JsonRpcMessage::Response(response) => {
                        Some(self.observer.untracked(&response.id))
                    }
                    JsonRpcMessage::Error(error) => Some(
                        error
                            .id
                            .as_ref()
                            .map_or_else(super::Correlation::default, |id| {
                                self.observer.untracked(id)
                            }),
                    ),
                    JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
                }
            } else {
                None
            },
            dispatch: Instant::now(),
        };
        let future = self.inner.send(message);
        async move {
            let result = future.await;
            let fallback = sending.fallback.take();
            if let Some(item) = sending.item.take() {
                sending
                    .observer
                    .sent(item, sending.dispatch, Instant::now(), result.is_err());
            } else if result.is_err() {
                sending.observer.sink.emit(super::Event::failure(
                    "unknown",
                    fallback.unwrap_or_default(),
                    sending.dispatch.elapsed(),
                    super::Fault::send_failed(),
                ));
            }
            result
        }
    }
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let observer = Arc::clone(&self.observer);
        let future = self.inner.receive();
        async move {
            let message = future.await;
            match &message {
                None => observer.input_ended(),
                Some(JsonRpcMessage::Request(request)) => {
                    let operation = match &request.request {
                        ClientRequest::CallToolRequest(request) => request.params.name.as_ref(),
                        _ => "unknown",
                    };
                    observer.requested(&request.id, operation, Instant::now());
                }
                Some(JsonRpcMessage::Notification(notification)) => {
                    if let ClientNotification::CancelledNotification(cancelled) =
                        &notification.notification
                        && let Some(id) = &cancelled.params.request_id
                    {
                        observer.cancelled(id);
                    }
                }
                _ => {}
            }
            message
        }
    }
    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let observer = Arc::clone(&self.observer);
        let future = self.inner.close();
        async move {
            observer.stage(ShutdownStage::TransportCloseStarted);
            let result = future.await;
            observer.stage(ShutdownStage::TransportCloseFinished);
            result
        }
    }
}
