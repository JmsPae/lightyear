//! Specify how a Server sends/receives messages with a Client
use bevy::utils::{Duration, EntityHashMap, Entry, HashMap};
use std::rc::Rc;

use crate::_reexport::{
    EntityUpdatesChannel, InputMessageKind, MessageBehaviour, MessageKind, MessageProtocol,
    PingChannel,
};
use anyhow::{Context, Result};
use bevy::ecs::component::Tick as BevyTick;
use bevy::prelude::{Entity, World};
use serde::{Deserialize, Serialize};
use tracing::{debug, debug_span, info, trace, trace_span};

use crate::channel::senders::ChannelSend;
use crate::client::sync::SyncConfig;
use crate::connection::events::{ConnectionEvents, IterMessageEvent};
use crate::connection::message::{ClientMessage, ServerMessage};
use crate::inputs::native::input_buffer::{InputBuffer, InputMessage};
use crate::netcode::ClientId;
use crate::packet::message_manager::MessageManager;
use crate::packet::message_receivers::MessageReceiver;
use crate::packet::message_sender::MessageSender;
use crate::packet::packet_manager::Payload;
use crate::prelude::{ChannelKind, DefaultUnorderedUnreliableChannel, MapEntities};
use crate::protocol::channel::ChannelRegistry;
use crate::protocol::Protocol;
use crate::serialize::reader::ReadBuffer;
use crate::server::events::ServerEvents;
use crate::shared::ping::manager::{PingConfig, PingManager};
use crate::shared::ping::message::SyncMessage;
use crate::shared::replication::components::{NetworkTarget, Replicate};
use crate::shared::replication::receive::ReplicationReceiver;
use crate::shared::replication::send::ReplicationSender;
use crate::shared::replication::ReplicationMessage;
use crate::shared::replication::ReplicationMessageData;
use crate::shared::tick_manager::Tick;
use crate::shared::tick_manager::TickManager;
use crate::shared::time_manager::TimeManager;

pub struct ConnectionManager<P: Protocol> {
    pub(crate) connections: HashMap<ClientId, Connection<P>>,
    channel_registry: ChannelRegistry,
    pub(crate) events: ServerEvents<P>,

    // NOTE: we put this here because we only need one per world, not one per connection
    /// Stores the last `Replicate` component for each replicated entity owned by the current world (the world that sends replication updates)
    /// Needed to know the value of the Replicate component after the entity gets despawned, to know how we replicate the EntityDespawn
    pub replicate_component_cache: EntityHashMap<Entity, Replicate<P>>,

    // list of clients that connected since the last time we sent replication messages
    // (we want to keep track of them because we need to replicate the entire world state to them)
    pub(crate) new_clients: Vec<ClientId>,
}

impl<P: Protocol> ConnectionManager<P> {
    pub fn new(channel_registry: ChannelRegistry) -> Self {
        Self {
            connections: HashMap::default(),
            channel_registry,
            events: ServerEvents::new(),
            replicate_component_cache: EntityHashMap::default(),
            new_clients: vec![],
        }
    }

    pub(crate) fn connection(&self, client_id: ClientId) -> Result<&Connection<P>> {
        self.connections
            .get(&client_id)
            .context("client id not found")
    }

    pub(crate) fn connection_mut(&mut self, client_id: ClientId) -> Result<&mut Connection<P>> {
        self.connections
            .get_mut(&client_id)
            .context("client id not found")
    }

    pub(crate) fn update(&mut self, time_manager: &TimeManager, tick_manager: &TickManager) {
        self.connections.values_mut().for_each(|connection| {
            connection.update(time_manager, tick_manager);
        });
    }

    pub(crate) fn add(&mut self, client_id: ClientId, ping_config: &PingConfig) {
        if let Entry::Vacant(e) = self.connections.entry(client_id) {
            #[cfg(feature = "metrics")]
            metrics::increment_gauge!("connected_clients", 1.0);

            debug!("New connection from id: {}", client_id);
            let mut connection = Connection::new(&self.channel_registry, ping_config);
            connection.events.push_connection();
            self.new_clients.push(client_id);
            e.insert(connection);
        } else {
            info!("Client {} was already in the connections list", client_id);
        }
    }

    pub(crate) fn remove(&mut self, client_id: ClientId) {
        #[cfg(feature = "metrics")]
        metrics::decrement_gauge!("connected_clients", 1.0);

        info!("Client {} disconnected", client_id);
        self.events.push_disconnects(client_id);
        self.connections.remove(&client_id);
    }

    /// Get the inputs for all clients for the given tick
    pub(crate) fn pop_inputs(
        &mut self,
        tick: Tick,
    ) -> impl Iterator<Item = (Option<P::Input>, ClientId)> + '_ {
        self.connections
            .iter_mut()
            .map(move |(client_id, connection)| {
                let received_input = connection.input_buffer.pop(tick);
                let fallback = received_input.is_none();

                // NOTE: if there is no input for this tick, we should use the last input that we have
                //  as a best-effort fallback.
                let input = match received_input {
                    None => connection.last_input.clone(),
                    Some(i) => {
                        connection.last_input = Some(i.clone());
                        Some(i)
                    }
                };
                if fallback {
                    // TODO: do not log this while clients are syncing..
                    debug!(
                    ?client_id,
                    ?tick,
                    fallback_input = ?&input,
                    "Missed client input!"
                    )
                }
                // TODO: We should also let the user know that it needs to send inputs a bit earlier so that
                //  we have more of a buffer. Send a SyncMessage to tell the user to speed up?
                //  See Overwatch GDC video
                (input, *client_id)
            })
    }

    pub(crate) fn buffer_message(
        &mut self,
        message: P::Message,
        channel: ChannelKind,
        target: NetworkTarget,
    ) -> Result<()> {
        // Rc is fine because the copies are all created on the same thread
        // let message = Rc::new(message);
        self.connections
            .iter_mut()
            .filter(|(id, _)| target.should_send_to(id))
            // TODO: here we should avoid the clone, it's the same message.. just use Rc?
            //  need to update the ServerMessage enum to use Rc<P::Message>!
            //  or serialize first, so we can use Bytes? where would the buffer be?
            .try_for_each(|(_, c)| c.buffer_message(message.clone(), channel))
    }

    pub(crate) fn buffer_replication_messages(&mut self, tick: Tick) -> Result<()> {
        let _span = trace_span!("buffer_replication_messages").entered();
        self.connections
            .values_mut()
            .try_for_each(move |c| c.buffer_replication_messages(tick))
    }

    pub fn receive(&mut self, world: &mut World, time_manager: &TimeManager) -> Result<()> {
        let mut messages_to_rebroadcast = vec![];
        self.connections
            .iter_mut()
            .for_each(|(client_id, connection)| {
                let _span = trace_span!("receive", ?client_id).entered();
                // receive
                let events = connection.receive(world, time_manager);
                self.events.push_events(*client_id, events);

                // rebroadcast messages
                messages_to_rebroadcast
                    .extend(std::mem::take(&mut connection.messages_to_rebroadcast));
            });
        for (message, target, channel_kind) in messages_to_rebroadcast {
            self.buffer_message(message, channel_kind, target)?;
        }
        Ok(())
    }
}

/// Wrapper that handles the connection between the server and a client
pub struct Connection<P: Protocol> {
    pub message_manager: MessageManager,
    pub(crate) replication_sender: ReplicationSender<P>,
    pub(crate) replication_receiver: ReplicationReceiver<P>,
    pub(crate) events: ConnectionEvents<P>,

    pub(crate) ping_manager: PingManager,
    /// Stores the inputs that we have received from the client.
    pub(crate) input_buffer: InputBuffer<P::Input>,
    /// Stores the last input we have received from the client.
    /// In case we are missing the client input for a tick, we will fallback to using this.
    pub(crate) last_input: Option<P::Input>,
    // TODO: maybe don't do any replication until connection is synced?

    // messages that we have received that need to be rebroadcasted to other clients
    pub(crate) messages_to_rebroadcast: Vec<(P::Message, NetworkTarget, ChannelKind)>,
}

impl<P: Protocol> Connection<P> {
    pub(crate) fn new(channel_registry: &ChannelRegistry, ping_config: &PingConfig) -> Self {
        // create the message manager and the channels
        let mut message_manager = MessageManager::new(channel_registry);
        // get the acks-tracker for entity updates
        let update_acks_tracker = message_manager
            .channels
            .get_mut(&ChannelKind::of::<EntityUpdatesChannel>())
            .unwrap()
            .sender
            .subscribe_acks();
        let replication_sender = ReplicationSender::new(update_acks_tracker);
        let replication_receiver = ReplicationReceiver::new();
        Self {
            message_manager,
            replication_sender,
            replication_receiver,
            ping_manager: PingManager::new(ping_config),
            input_buffer: InputBuffer::default(),
            last_input: None,
            events: ConnectionEvents::default(),
            messages_to_rebroadcast: vec![],
        }
    }

    pub(crate) fn update(&mut self, time_manager: &TimeManager, tick_manager: &TickManager) {
        self.message_manager
            .update(time_manager, &self.ping_manager, tick_manager);
        self.ping_manager.update(time_manager);
    }

    pub(crate) fn buffer_message(
        &mut self,
        message: P::Message,
        channel: ChannelKind,
    ) -> Result<()> {
        // TODO: i know channel names never change so i should be able to get them as static
        // TODO: just have a channel registry enum as well?
        let channel_name = self
            .message_manager
            .channel_registry
            .name(&channel)
            .unwrap_or("unknown")
            .to_string();
        let message = ServerMessage::<P>::Message(message);
        message.emit_send_logs(&channel_name);
        self.message_manager.buffer_send(message, channel)?;
        Ok(())
    }

    pub(crate) fn buffer_replication_messages(&mut self, tick: Tick) -> Result<()> {
        self.replication_sender
            .finalize(tick)
            .into_iter()
            .try_for_each(|(channel, group_id, message_data)| {
                let should_track_ack = matches!(message_data, ReplicationMessageData::Updates(_));
                let channel_name = self
                    .message_manager
                    .channel_registry
                    .name(&channel)
                    .unwrap_or("unknown")
                    .to_string();
                let message = ClientMessage::<P>::Replication(ReplicationMessage {
                    group_id,
                    data: message_data,
                });
                message.emit_send_logs(&channel_name);
                let message_id = self
                    .message_manager
                    .buffer_send(message, channel)?
                    .expect("The EntityUpdatesChannel should always return a message_id");

                // keep track of the group associated with the message, so we can handle receiving an ACK for that message_id later
                if should_track_ack {
                    self.replication_sender
                        .updates_message_id_to_group_id
                        .insert(message_id, group_id);
                }
                Ok(())
            })
    }

    /// Send packets that are ready to be sent
    pub fn send_packets(
        &mut self,
        time_manager: &TimeManager,
        tick_manager: &TickManager,
    ) -> Result<Vec<Payload>> {
        // update the ping manager with the actual send time
        // TODO: issues here: we would like to send the ping/pong messages immediately, otherwise the recorded current time is incorrect
        //   - can give infinity priority to this channel?
        //   - can write directly to io otherwise?
        if time_manager.is_ready_to_send() {
            // maybe send pings
            // same thing, we want the correct send time for the ping
            // (and not have the delay between when we prepare the ping and when we send the packet)
            if let Some(ping) = self.ping_manager.maybe_prepare_ping(time_manager) {
                trace!("Sending ping {:?}", ping);
                let message = ServerMessage::<P>::Sync(SyncMessage::Ping(ping));
                let channel = ChannelKind::of::<PingChannel>();
                self.message_manager.buffer_send(message, channel)?;
            }

            // prepare the pong messages with the correct send time
            self.ping_manager
                .take_pending_pongs()
                .into_iter()
                .try_for_each(|mut pong| {
                    trace!("Sending pong {:?}", pong);
                    // update the send time of the pong
                    pong.pong_sent_time = time_manager.current_time();
                    let message = ServerMessage::<P>::Sync(SyncMessage::Pong(pong));
                    let channel = ChannelKind::of::<PingChannel>();
                    self.message_manager.buffer_send(message, channel)?;
                    Ok::<(), anyhow::Error>(())
                })?;
        }
        self.message_manager
            .send_packets(tick_manager.current_tick())
    }

    pub fn receive(
        &mut self,
        world: &mut World,
        time_manager: &TimeManager,
    ) -> ConnectionEvents<P> {
        let _span = trace_span!("receive").entered();
        for (channel_kind, messages) in self.message_manager.read_messages::<ClientMessage<P>>() {
            let channel_name = self
                .message_manager
                .channel_registry
                .name(&channel_kind)
                .unwrap_or("unknown");
            let _span_channel = trace_span!("channel", channel = channel_name).entered();

            if !messages.is_empty() {
                trace!(?channel_name, "Received messages");
                for (tick, message) in messages.into_iter() {
                    match message {
                        ClientMessage::Message(mut message, target) => {
                            // map any entities inside the message
                            message.map_entities(Box::new(
                                &self.replication_receiver.remote_entity_map,
                            ));
                            if target != NetworkTarget::None {
                                self.messages_to_rebroadcast.push((
                                    message.clone(),
                                    target,
                                    channel_kind,
                                ));
                            }
                            // don't put InputMessage into events else the events won't be classified as empty
                            match message.input_message_kind() {
                                #[cfg(feature = "leafwing")]
                                InputMessageKind::Leafwing => {
                                    trace!("received input message, pushing it to events");
                                    self.events.push_input_message(message);
                                }
                                InputMessageKind::Native => {
                                    trace!("update input buffer");
                                    let input_message = message.try_into().unwrap();
                                    // info!("Received input message: {:?}", input_message);
                                    self.input_buffer.update_from_message(input_message);
                                }
                                InputMessageKind::None => {
                                    // buffer the message
                                    self.events.push_message(channel_kind, message);
                                }
                            }
                        }
                        ClientMessage::Replication(replication) => {
                            // buffer the replication message
                            self.replication_receiver.recv_message(replication, tick);
                        }
                        ClientMessage::Sync(ref sync) => {
                            match sync {
                                SyncMessage::Ping(ping) => {
                                    // prepare a pong in response (but do not send yet, because we need
                                    // to set the correct send time)
                                    self.ping_manager.buffer_pending_pong(ping, time_manager);
                                }
                                SyncMessage::Pong(pong) => {
                                    // process the pong
                                    self.ping_manager.process_pong(pong, time_manager);
                                }
                            }
                        }
                    }
                }
                // Check if we have any replication messages we can apply to the World (and emit events)
                for (group, replication_list) in self.replication_receiver.read_messages() {
                    trace!(?group, ?replication_list, "read replication messages");
                    replication_list.into_iter().for_each(|(_, replication)| {
                        // TODO: we could include the server tick when this replication_message was sent.
                        self.replication_receiver.apply_world(
                            world,
                            replication,
                            group,
                            &mut self.events,
                        );
                    });
                }
            }
        }

        // TODO: do i really need this? I could just create events in this function directly?
        //  why do i need to make events a field of the connection?
        //  is it because of push_connection?
        std::mem::replace(&mut self.events, ConnectionEvents::new())
    }

    pub fn recv_packet(
        &mut self,
        reader: &mut impl ReadBuffer,
        tick_manager: &TickManager,
        bevy_tick: BevyTick,
    ) -> Result<()> {
        self.replication_sender.recv_update_acks(bevy_tick);
        let tick = self.message_manager.recv_packet(reader)?;
        debug!("Received server packet with tick: {:?}", tick);
        Ok(())
    }
}
