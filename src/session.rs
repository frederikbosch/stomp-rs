use std::collections::hash_map::HashMap;
use std::mem;
use std::thread;
use std::ops::DerefMut;
use std::io::Read;
use std::io::Write;
use std::io::Result;
use std::io::Error;
use std::io::ErrorKind::{Other};
use std::marker::PhantomData;
use connection::Connection;
use subscription::AckMode;
use subscription::AckMode::{Auto, Client, ClientIndividual};
use subscription::AckOrNack;
use subscription::AckOrNack::{Ack, Nack};
use subscription::{Subscription, MessageHandler, ToMessageHandler};
use frame::Frame;
use frame::ToFrameBody;
use frame::Transmission::{HeartBeat, CompleteFrame, ConnectionClosed};
use header;
use header::HeaderList;
use header::ReceiptId;
use header::StompHeaderSet;
use transaction::Transaction;
use session_builder::SessionBuilder;
use message_builder::MessageBuilder;
use subscription_builder::SubscriptionBuilder;
use frame_buffer::FrameBuffer;

use mio::{EventLoop, Handler, Token, ReadHint, Timeout};

pub trait FrameHandler {
  fn on_frame(&mut self, &Frame);
}

pub trait FrameHandlerMut {
	fn on_frame(&mut self, &mut Frame);
}

impl <F> FrameHandler for F where F: FnMut(&Frame) {
  fn on_frame(&mut self, frame: &Frame) {
    self(frame)
  }
}

impl <F> FrameHandlerMut for F where F: FnMut(&mut Frame) {
	fn on_frame(&mut self, frame: &mut Frame) {
		self(frame)
	}
}

pub trait ToFrameHandler <'a> {
  fn to_frame_handler(self) -> Box<FrameHandler + 'a>;
}

pub trait ToFrameHandlerMut <'a> {
	fn to_frame_handler_mut(self) -> Box<FrameHandlerMut + 'a>;
}

impl <'a, T: 'a> ToFrameHandler <'a> for T where T: FrameHandler {
  fn to_frame_handler(self) -> Box<FrameHandler + 'a> {
    Box::new(self) as Box<FrameHandler>
  }
}

impl <'a, T: 'a> ToFrameHandlerMut <'a> for T where T: FrameHandlerMut {
  fn to_frame_handler_mut(self) -> Box<FrameHandlerMut + 'a> {
    Box::new(self) as Box<FrameHandlerMut>
  }
}

pub struct ReceiptHandler<'a, T> where T: 'a + ToFrameHandler<'a> {
  pub handler: T,
  _marker: PhantomData<&'a T>
}

impl<'a, T> ReceiptHandler<'a, T> where T: 'a + ToFrameHandler<'a> {
  pub fn new(val: T) -> ReceiptHandler<'a, T> {
    let r = ReceiptHandler { handler: val, _marker: PhantomData };
    r
  }
}

const READ_BUFFER_SIZE: usize = 64 * 1024;
const GRACE_PERIOD_MULTIPLIER: f64 = 2.0;

pub struct Session <'a> {
  session_builder: SessionBuilder<'a>,
  pub connection : Connection,
  read_buffer: Box<[u8; READ_BUFFER_SIZE]>,
  frame_buffer: FrameBuffer,
  next_transaction_id: u32,
  next_subscription_id: u32,
  next_receipt_id: u32,
  rx_heartbeat_ms: u64,
  rx_heartbeat_timeout: Option<Timeout>,
  tx_heartbeat_ms: u64,
  pub subscriptions: HashMap<String, Subscription <'a>>,
  pub receipt_handlers: HashMap<String, Box<FrameHandler + 'a>>,
  error_callback: Box<FrameHandler + 'a>,
	frame_send_callback: Box<FrameHandlerMut + 'a>,
	frame_receive_callback: Box<FrameHandlerMut + 'a>
}

pub enum StompTimeout {
  SendHeartBeat,
  ReceiveHeartBeat
}

impl <'a> Handler for Session<'a> {
  type Timeout = StompTimeout;
  type Message = ();

  fn timeout(&mut self, event_loop: &mut EventLoop<Session<'a>>, timeout: StompTimeout) {
    match timeout {
      StompTimeout::SendHeartBeat => self.send_heartbeat(event_loop),
      StompTimeout::ReceiveHeartBeat => {
        debug!("Did not receive a heartbeat in time.");
      },
    }
  }

  fn readable(&mut self, event_loop: &mut EventLoop<Session<'a>>, _token: Token, _: ReadHint) {
    debug!("Readable! Buffer size: {}", &mut self.read_buffer.len());
    debug!("Frame buffer length: {}", &mut self.frame_buffer.len());
    let bytes_read = match self.connection.tcp_stream.read(self.read_buffer.deref_mut()){
      Ok(0) => {
        info!("Read 0 bytes. Connection closed by remote host.");
        self.reconnect(event_loop);
        return;
      },
      Ok(bytes_read) => bytes_read,
      Err(error) => {
        info!("Error while reading: {}", error);
        self.reconnect(event_loop);
        return;
      },
    };
    info!("Read {} bytes", bytes_read);
    self.frame_buffer.append(&self.read_buffer[..bytes_read]);
    let mut num_frames = 0u32;
    loop {
      debug!("Reading from frame buffer");
      match self.frame_buffer.read_transmission() {
        Some(HeartBeat) => self.on_heartbeat(event_loop),
        Some(CompleteFrame(mut frame)) => {
          debug!("Received frame!:\n{}", frame);
          self.reset_rx_heartbeat_timeout(event_loop);
					self.frame_receive_callback.on_frame(&mut frame);
          self.dispatch(&mut frame);
          self.frame_buffer.recycle_frame(frame);
          num_frames += 1;
        },
        Some(ConnectionClosed) => {
          info!("Connection closed by remote host.");
          self.reconnect(event_loop);
        },
        None => {
          debug!("Done. Read {} frames.", num_frames);
          break;
        }
      }
    }
  } 
}

impl <'a> Session <'a> {
  pub fn new(session_builder: SessionBuilder<'a>, connection: Connection, tx_heartbeat_ms: u32, rx_heartbeat_ms: u32) -> Session<'a> {
    let modified_rx_heartbeat_ms : u32 = ((rx_heartbeat_ms as f64) * GRACE_PERIOD_MULTIPLIER) as u32;

    Session {
      session_builder: session_builder,
      connection: connection,
      frame_buffer: FrameBuffer::new(),
      //TODO: Make this configurable
      read_buffer: Box::new([0; READ_BUFFER_SIZE]),
      next_transaction_id: 0,
      next_subscription_id: 0,
      next_receipt_id: 0,
      rx_heartbeat_ms: modified_rx_heartbeat_ms as u64,
      rx_heartbeat_timeout: None,
      tx_heartbeat_ms: (tx_heartbeat_ms as f64 / 2f64) as u64, //FIXME: Make this configurable, change units
      subscriptions: HashMap::new(),
      receipt_handlers: HashMap::new(),
      error_callback: Box::new(Session::default_error_callback) as Box<FrameHandler>,
			frame_send_callback: Box::new(Session::default_frame_send_callback) as Box<FrameHandlerMut>,
			frame_receive_callback: Box::new(Session::default_frame_receive_callback) as Box<FrameHandlerMut>
    }
  }

  fn reconnect(&mut self, event_loop: &mut EventLoop<Session<'a>>) {
    let delay_between_attempts = 3_000u32; //TODO: Make this configurable
    event_loop.deregister(&self.connection.tcp_stream).ok().expect("Failed to deregister dead tcp connection.");
    self.clear_rx_heartbeat_timeout(event_loop);
    self.frame_buffer.reset();
    loop {
      match self.session_builder.clone().start() {
        Ok(session) => {
          info!("Reconnected successfully!");
          let subscriptions = mem::replace(&mut self.subscriptions, HashMap::new());
          mem::replace(self, session);
          self.subscriptions = subscriptions;
          event_loop.register(&self.connection.tcp_stream, Token(0)).ok().expect("Couldn't register re-established connection with the event loop.");
          self.register_rx_heartbeat_timeout(event_loop);
          self.reset_rx_heartbeat_timeout(event_loop);
          info!("Resubscribing to {} destinations", self.subscriptions.len());
          let frames : Vec<Frame> = self.subscriptions
            .values()
            .map(|subscription| {
              info!("Re-subscribing to '{}'", &subscription.destination);
              let mut subscribe_frame = Frame::subscribe(&subscription.id, &subscription.destination, subscription.ack_mode);
              subscribe_frame.headers.concat(&mut subscription.headers.clone());
              subscribe_frame.headers.retain(|header| (*header).get_key() != "receipt"); //TODO: Find a way to clean this up.
              subscribe_frame
            }).collect();
          for subscribe_frame in frames {
            self.send(subscribe_frame).ok().expect("Couldn't re-subscribe.");
          }
          break;
        },
        Err(error) => {
          info!("Failed to reconnect: {:?}, retrying again in {}ms", error, delay_between_attempts);
        }
      };
      debug!("Waiting {}ms before attempting to connect again.", delay_between_attempts);
      thread::sleep_ms(delay_between_attempts);
    }
  }

  fn register_tx_heartbeat_timeout(&self, event_loop: &mut EventLoop<Session<'a>>) {
    if self.tx_heartbeat_ms <= 0 {
      debug!("Heartbeat transmission ms is {}, no need to register a callback.", self.tx_heartbeat_ms);
      return;
    }
    let _ = event_loop.timeout_ms(StompTimeout::SendHeartBeat, self.tx_heartbeat_ms);
  }

  fn register_rx_heartbeat_timeout(&mut self, event_loop: &mut EventLoop<Session<'a>>) {
    if self.rx_heartbeat_ms <= 0 {
      debug!("Heartbeat receipt ms is {}, no need to register a callback.", self.tx_heartbeat_ms);
      return;
    }
    let timeout = event_loop
      .timeout_ms(StompTimeout::ReceiveHeartBeat, self.rx_heartbeat_ms)
      .ok()
      .expect("Could not register a timeout to receive a heartbeat.");
    self.rx_heartbeat_timeout = Some(timeout);
  }

  fn send_heartbeat(&mut self, event_loop: &mut EventLoop<Session<'a>>) {
    debug!("Sending heartbeat");
    self.connection.tcp_stream.write("\n".as_bytes()).ok().expect("Could not send a heartbeat. Connection failed.");
    let _ = self.connection.tcp_stream.flush();
    self.register_tx_heartbeat_timeout(event_loop);
  }

  fn on_heartbeat(&mut self, event_loop: &mut EventLoop<Session<'a>>) {
    debug!("Received HeartBeat");
    self.reset_rx_heartbeat_timeout(event_loop);
  }

  fn reset_rx_heartbeat_timeout(&mut self, event_loop: &mut EventLoop<Session<'a>>) {
    debug!("Resetting heartbeat rx timeout");
    self.clear_rx_heartbeat_timeout(event_loop);
    self.register_rx_heartbeat_timeout(event_loop);
  }

  fn clear_rx_heartbeat_timeout(&mut self, event_loop: &mut EventLoop<Session<'a>>) {
    debug!("Clearing existing heartbeat rx timeout");
    self.rx_heartbeat_timeout.map(|timeout| {
      let result = event_loop.clear_timeout(timeout);
      debug!("Reset complete -> {}", result);
    });
  }

  fn default_error_callback(frame : &Frame) {
    error!("ERROR received:\n{}", frame);
  }

	fn default_frame_send_callback(frame : &mut Frame) {
		debug!("FRAME sending:\n{}", frame);
	}

	fn default_frame_receive_callback(frame : &mut Frame) {
		debug!("FRAME received:\n{}", frame);
	}

  pub fn on_error<T: 'a>(&mut self, handler_convertible: T) where T : ToFrameHandler<'a> + 'a {
    let handler = handler_convertible.to_frame_handler();
    self.error_callback = handler;
  }

	pub fn on_before_send<T: 'a>(&mut self, handler_convertible: T) where T : ToFrameHandlerMut<'a> + 'a {
		let handler = handler_convertible.to_frame_handler_mut();
		self.frame_send_callback = handler;
	}

	pub fn on_before_receive<T: 'a>(&mut self, handler_convertible: T) where T : ToFrameHandlerMut<'a> + 'a {
		let handler = handler_convertible.to_frame_handler_mut();
		self.frame_receive_callback = handler;
	}

  fn handle_receipt(&mut self, frame: &mut Frame) {
    match frame.headers.get_receipt_id() {
      Some(ReceiptId(ref receipt_id)) => {
        let mut handler = match self.receipt_handlers.remove(*receipt_id) {
          Some(handler) => {
            debug!("Calling handler for ReceiptId '{}'.", *receipt_id);
            handler
          },
          None => panic!("Received unexpected RECEIPT '{}'", *receipt_id)
        };
        handler.on_frame(&frame);
      },
      None => panic!("Received RECEIPT frame without a receipt-id")
    };
  }

  pub fn outstanding_receipts(&self) -> Vec<&str> {
    self.receipt_handlers.keys().map(|key| key.as_ref()).collect()
  }

  fn generate_transaction_id(&mut self) -> u32 {
    let id = self.next_transaction_id;
    self.next_transaction_id += 1;
    id
  }

  pub fn generate_subscription_id(&mut self) -> u32 {
    let id = self.next_subscription_id;
    self.next_subscription_id += 1;
    id
  }

  pub fn generate_receipt_id(&mut self) -> u32 {
    let id = self.next_receipt_id;
    self.next_receipt_id += 1;
    id
  }

  pub fn message<'b, T: ToFrameBody> (&'b mut self, destination: &str, body_convertible: T) -> MessageBuilder<'b, 'a> {
    let send_frame = Frame::send(destination, body_convertible.to_frame_body());
    MessageBuilder {
     session: self,
     frame: send_frame
    }
  }

  pub fn subscription<'b, 'c: 'a, T>(&'b mut self, destination: &'b str, handler_convertible: T) -> SubscriptionBuilder<'b, 'a, 'c> where T: ToMessageHandler<'c> {
    let message_handler : Box<MessageHandler> = handler_convertible.to_message_handler();
    SubscriptionBuilder{
      session: self,
      destination: destination,
      ack_mode: AckMode::Auto,
      handler: message_handler,
      headers: HeaderList::new(),
    }
  }

  pub fn unsubscribe(&mut self, sub_id: &str) -> Result<()> {
     let _ = self.subscriptions.remove(sub_id);
     let unsubscribe_frame = Frame::unsubscribe(sub_id.as_ref());
     self.send(unsubscribe_frame)
  }

  pub fn disconnect(&mut self) -> Result<()> {
    let disconnect_frame = Frame::disconnect();
    self.send(disconnect_frame)
  }

  pub fn begin_transaction<'b>(&'b mut self) -> Result<Transaction<'b, 'a>> {
    let mut transaction = Transaction::new(self.generate_transaction_id(), self);
    let _ = try!(transaction.begin());
    Ok(transaction)
  }

  pub fn send(&mut self, frame: Frame) -> Result<()> {
		let mut mut_frame = frame;
		self.frame_send_callback.on_frame(&mut mut_frame);
    match mut_frame.write(&mut self.connection.tcp_stream) {
      Ok(_) => Ok(()),//FIXME: Replace 'Other' below with a more meaningful ErrorKind
      Err(_) => Err(Error::new(Other, "Could not send frame: the connection to the server was lost."))
    }
  }

  pub fn dispatch(&mut self, frame: &mut Frame) {
    // Check for ERROR frame
    match frame.command.as_ref() {
       "ERROR" => return self.error_callback.on_frame(&frame),
       "RECEIPT" => return self.handle_receipt(frame),
        _ => {} // No operation
    };
 
    let ack_mode : AckMode;
    let callback_result : AckOrNack; 
    { // This extra scope is required to free up `frame` and `self.subscriptions`
      // following a borrow.

      // Find the subscription ID on the frame that was received
      let header::Subscription(sub_id) = 
        frame.headers
        .get_subscription()
        .expect("Frame did not contain a subscription header.");

      // Look up the appropriate Subscription object
      let subscription = 
         self.subscriptions
         .get_mut(sub_id)
         .expect("Received a message for an unknown subscription.");

      // Take note of the ack_mode used by this Subscription
      ack_mode = subscription.ack_mode;
      // Invoke the callback in the Subscription, providing the frame
      // Take note of whether this frame should be ACKed or NACKed
      callback_result = (*subscription.handler).on_message(&frame);
    }

    debug!("Executing.");
    match ack_mode {
      Auto => {
        debug!("Auto ack, no frame sent.");
      }
      Client | ClientIndividual => {
        let header::Ack(ack_id) = 
          frame.headers
          .get_ack()
          .expect("Message did not have an 'ack' header.");
        match callback_result {
          Ack =>  self.acknowledge_frame(ack_id),
          Nack => self.negatively_acknowledge_frame(ack_id)
        }.unwrap_or_else(|error|panic!(format!("Could not acknowledge frame: {}", error)));
      } // Client | ...
    }
  } 

  fn acknowledge_frame(&mut self, ack_id: &str) -> Result<()> {
    let ack_frame = Frame::ack(ack_id);
    self.send(ack_frame)
  }

  fn negatively_acknowledge_frame(&mut self, ack_id: &str) -> Result<()>{
    let nack_frame = Frame::nack(ack_id);
    self.send(nack_frame)
  }

  pub fn listen(&mut self) -> Result<()> {
    let mut event_loop : EventLoop<Session<'a>> = EventLoop::new().unwrap();
    let _ = event_loop.register(&self.connection.tcp_stream, Token(0));
    self.register_tx_heartbeat_timeout(&mut event_loop);
    self.register_rx_heartbeat_timeout(&mut event_loop);
    event_loop.run(self)
  }
}
