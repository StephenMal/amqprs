//! API implementation of AMQP Channel
//!

use std::{
    collections::{HashMap, VecDeque},
    ops::Deref,
};


use tokio::{
    sync::{mpsc, oneshot},
    task::yield_now,
};

use crate::{
    api::{callbacks::ChannelCallback},
    frame::{CloseChannelOk, Frame, MethodHeader},
    net::{IncomingMessage, ConnManagementCommand},
};
use tracing::{debug, trace, error};

use super::{Channel, ConsumerMessage, DispatcherManagementCommand};

struct ConsumerResource {
    fifo: VecDeque<ConsumerMessage>,
    tx: Option<mpsc::Sender<ConsumerMessage>>,
}

impl ConsumerResource {
    fn new() -> Self {
        Self {
            fifo: VecDeque::new(),
            tx: None,
        }
    }
    fn register_tx(
        &mut self,
        tx: mpsc::Sender<ConsumerMessage>,
    ) -> Option<mpsc::Sender<ConsumerMessage>> {
        self.tx.replace(tx)
    }
    fn unregister_tx(&mut self) -> Option<mpsc::Sender<ConsumerMessage>> {
        self.tx.take()
    }

    fn get_tx(&self) -> Option<&mpsc::Sender<ConsumerMessage>> {
        self.tx.as_ref()
    }

    fn push(&mut self, message: ConsumerMessage) {
        self.fifo.push_back(message);
    }
    fn pop(&mut self) -> Option<ConsumerMessage> {
        self.fifo.pop_front()
    }
}

enum State {
    Initial,
    Deliver,
    GetOk,
    GetEmpty,
    Return,
}
pub(crate) struct ChannelDispatcher {
    channel: Channel,
    dispatcher_rx: mpsc::Receiver<IncomingMessage>,
    dispatcher_mgmt_rx: mpsc::Receiver<DispatcherManagementCommand>,
    consumers: HashMap<String, ConsumerResource>,
    get_content_responder: Option<mpsc::Sender<IncomingMessage>>,
    responders: HashMap<&'static MethodHeader, oneshot::Sender<IncomingMessage>>,
    callback: Option<Box<dyn ChannelCallback + Send + 'static>>,
    state: State,
}
/////////////////////////////////////////////////////////////////////////////
impl ChannelDispatcher {
    pub(crate) fn new(
        channel: Channel,
        dispatcher_rx: mpsc::Receiver<IncomingMessage>,
        dispatcher_mgmt_rx: mpsc::Receiver<DispatcherManagementCommand>,
    ) -> Self {
        Self {
            channel,
            dispatcher_rx,
            dispatcher_mgmt_rx,
            consumers: HashMap::new(),
            get_content_responder: None,
            responders: HashMap::new(),
            callback: None,
            state: State::Initial,
        }
    }
    fn get_or_new_consumer(&mut self, consumer_tag: &String) -> &mut ConsumerResource {
        if !self.consumers.contains_key(consumer_tag) {
            let resource = ConsumerResource::new();
            self.consumers.insert(consumer_tag.clone(), resource);
        }
        self.consumers.get_mut(consumer_tag).unwrap()
    }

    fn remove_consumer(&mut self, consumer_tag: &String) -> Option<ConsumerResource> {
        self.consumers.remove(consumer_tag)
    }
    pub(in crate::api) async fn spawn(mut self) {
        tokio::spawn(async move {
            // internal state

            // let channel_id = self.channel.channel_id();
            // // buffer pool for all consumers
            // let mut consumers = ConsumerBuffersPool::new();
            // // single message buffer
            let mut message_buffer = ConsumerMessage {
                deliver: None,
                basic_properties: None,
                content: None,
            };
            // // responders for Get content and synchronous response
            // let mut get_responder = None;
            // let mut oneshot_responders: HashMap<
            //     &'static MethodHeader,
            //     oneshot::Sender<IncomingMessage>,
            // > = HashMap::new();

            // let mut callback = None;

            // // initial state
            // let mut state = State::Initial;

            trace!(
                "Dispatcher of channel {} starts!",
                self.channel.channel_id()
            );

            loop {
                tokio::select! {
                    biased;

                    command = self.dispatcher_mgmt_rx.recv() => {
                        // handle command channel error
                        let cmd = match command {
                            None => break,
                            Some(v) => v,
                        };
                        // handle command
                        match cmd {
                            DispatcherManagementCommand::RegisterContentConsumer(cmd) => {
                                // TODO: check insert result
                                trace!("AsyncConsumer: {}, tx registered!", cmd.consumer_tag);
                                let consumer = self.get_or_new_consumer(&cmd.consumer_tag);
                                consumer.register_tx(cmd.consumer_tx);
                                // forward buffered messages
                                while !consumer.fifo.is_empty() {
                                    trace!("Total buffered messages: {}", consumer.fifo.len());
                                    let msg = consumer.pop().unwrap();
                                    consumer.get_tx().unwrap().send(msg).await.unwrap();
                                }

                            },
                            DispatcherManagementCommand::UnregisterContentConsumer(cmd) => {
                                // TODO: check remove result
                                self.remove_consumer(&cmd.consumer_tag);

                            },
                            DispatcherManagementCommand::RegisterGetContentResponder(cmd) => {
                                self.get_content_responder.replace(cmd.tx);
                            }
                            DispatcherManagementCommand::RegisterOneshotResponder(cmd) => {
                                self.responders.insert(cmd.method_header, cmd.responder);
                                cmd.acker.send(()).unwrap();
                            }
                            DispatcherManagementCommand::RegisterChannelCallback(cmd) => {
                                self.callback.replace(cmd.callback);
                            }
                        }
                    }
                    message = self.dispatcher_rx.recv() => {
                        // handle message channel error
                        let frame = match message {
                            None => break,
                            Some(v) => v,
                        };
                        // handle frames
                        match frame {
                            Frame::Return(_, method) => {
                                self.state = State::Return;
                                debug!("returned : {}, {}", method.reply_code, method.reply_text.deref());
                            }
                            Frame::GetEmpty(_, get_empty) => {
                                self.state = State::GetEmpty;
                                if let Err(err) = self.get_content_responder.take().expect("Get responder must be registered").send(get_empty.into_frame()).await {
                                    debug!("Failed to dispatch GetEmpty frame, cause: {}", err);
                                }
                            }
                            Frame::GetOk(_, get_ok) => {
                                self.state = State::GetOk;
                                if let Err(err) = self.get_content_responder.as_ref().expect("Get responder must be registered").send(get_ok.into_frame()).await {
                                    debug!("Failed to dispatch GetOk frame, cause: {}", err);
                                }
                            }
                            Frame::Deliver(_, deliver) => {

                                self.state = State::Deliver;
                                message_buffer.deliver = Some(deliver);
                            }
                            Frame::ContentHeader(header) => {
                                match self.state {
                                    State::Deliver => message_buffer.basic_properties = Some(header.basic_properties),
                                    State::GetOk => {
                                        if let Err(err) = self.get_content_responder.as_ref().expect("Get responder must be registered").send(header.into_frame()).await {
                                            debug!("Failed to dispatch GetOk ContentHeader frame, cause: {}", err);
                                        }
                                    },
                                    State::Return => todo!("handle Return content"),
                                    State::Initial | State::GetEmpty  => unreachable!("invalid dispatcher state"),
                                }

                            }
                            Frame::ContentBody(body) => {
                                match self.state {
                                    State::Deliver => {
                                        message_buffer.content = Some(body.inner);

                                        let consumer_tag = message_buffer.deliver.as_ref().unwrap().consumer_tag().clone();
                                        let consumer_message  = ConsumerMessage {
                                            deliver: message_buffer.deliver.take(),
                                            basic_properties: message_buffer.basic_properties.take(),
                                            content: message_buffer.content.take(),
                                        };
                                        let consumer = self.get_or_new_consumer(&consumer_tag);
                                        match consumer.get_tx() {
                                            Some(consumer_tx) => {
                                                if let Err(_) = consumer_tx.send(consumer_message).await {
                                                    debug!("Failed to dispatch message to consumer {}", consumer_tag);
                                                }
                                            },
                                            None => {
                                                debug!("Can't find consumer '{}', buffering message", consumer_tag);
                                                consumer.push(consumer_message);
                                                // FIXME: try to yield for registering consumer
                                                //      not sure if it is necessary
                                                yield_now().await;
                                            },
                                        };
                                    }
                                    State::GetOk => {
                                        if let Err(err) = self.get_content_responder.take().expect("Get responder must be registered").send(body.into_frame()).await {
                                            debug!("Failed to dispatch GetOk ContentBody frame, cause: {}", err);
                                        }
                                    },
                                    State::Return => todo!("handle Return content"),
                                    State::Initial | State::GetEmpty  => unreachable!("invalid dispatcher state"),
                                }


                            }
                            // Close channel response from server
                            Frame::CloseChannelOk(method_header, close_channel_ok) => {
                                self.responders.remove(method_header)
                                .expect("CloseChannelOk responder must be registered")
                                .send(close_channel_ok.into_frame()).unwrap();

                                self.channel.set_open_state(false);
                                break;
                            }
                            // TODO:
                            | Frame::FlowOk(method_header, _)
                            // | Frame::RequestOk(method_header, _) // Deprecated
                            | Frame::DeclareOk(method_header, _)
                            | Frame::DeleteOk(method_header, _)
                            | Frame::BindOk(method_header, _)
                            | Frame::UnbindOk(method_header, _)
                            | Frame::DeclareQueueOk(method_header, _)
                            | Frame::BindQueueOk(method_header, _)
                            | Frame::PurgeQueueOk(method_header, _)
                            | Frame::DeleteQueueOk(method_header, _)
                            | Frame::UnbindQueueOk(method_header, _)
                            | Frame::QosOk(method_header, _)
                            | Frame::ConsumeOk(method_header, _)
                            | Frame::CancelOk(method_header, _)
                            | Frame::RecoverOk(method_header, _)
                            | Frame::SelectOk(method_header, _)
                            | Frame::TxSelectOk(method_header, _)
                            | Frame::TxCommitOk(method_header, _)
                            | Frame::TxRollbackOk(method_header, _) => {
                                // handle synchronous response
                                match self.responders.remove(method_header)
                                {
                                    Some(responder) => {
                                        if let Err(response) = responder.send(frame) {
                                            debug!(
                                                "Failed to forward response frame {} to channel {}",
                                                response, self.channel.channel_id()
                                            );
                                        }
                                    }
                                    None => debug!(
                                        "No responder to forward frame {} to channel {}",
                                        frame, self.channel.channel_id()
                                    ),
                                }

                            }
                            //////////////////////////////////////////////////////////
                            // Method frames of asynchronous request

                            // Server request to close channel
                            Frame::CloseChannel(_method_header, close_channel) => {
                                self.channel.set_open_state(false);
                                // first, respond to server that we have received the request
                                self.channel.shared.outgoing_tx
                                .send((self.channel.channel_id(), CloseChannelOk::default().into_frame()))
                                .await.unwrap();

                                // callback
                                if let Some(mut cb) = self.callback {
                                    cb.close(&self.channel, close_channel).await;
                                }
                                break;
                            }
                            // TODO
                            | Frame::Flow(_method_header, _)
                            | Frame::Cancel(_method_header, _)
                            | Frame::Ack(_method_header, _) // confirmed mode
                            | Frame::Nack(_method_header, _) => {
                                todo!("handle asynchronous request")
                            }
                            _ => unreachable!("Not acceptable frame for dispatcher: {:?}", frame),
                        }
                    }
                    else => {
                        break;
                    }

                }
            }
            let cmd = ConnManagementCommand::UnregisterChannelResource(self.channel.channel_id());
            debug!("Request to unregister channel resource {}", self.channel.channel_id());
            if let Err(err) = self.channel.shared.conn_mgmt_tx.send(cmd).await {
                error!("Failed to unregister channel resource, cause: {}", err);
            }
            debug!("Exit dispatcher of channel {}", self.channel.channel_id());
        });
    }
}
