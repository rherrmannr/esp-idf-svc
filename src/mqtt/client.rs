use core::convert::TryInto;
use core::{ptr, slice, time};

extern crate alloc;
use alloc::{borrow::Cow, sync::Arc};

use embedded_svc::{mqtt::client, service};

use esp_idf_hal::mutex::{Condvar, Mutex};

use esp_idf_sys::*;

use crate::private::{common::Newtype, cstr::*};

// !!! NOTE: WORK IN PROGRESS

#[derive(Debug)]
pub struct LwtConfiguration<'a> {
    pub topic: &'a str,
    pub payload: &'a [u8],
    pub qos: client::QoS,
    pub retain: bool,
}

#[derive(Debug, Default)]
pub struct Configuration<'a> {
    // pub protocol_version: ProtocolVersion,
    pub client_id: Option<&'a str>,

    pub connection_refresh_interval: time::Duration,
    pub keep_alive_interval: Option<time::Duration>,
    pub reconnect_timeout: time::Duration,
    pub network_timeout: time::Duration,

    pub lwt: Option<LwtConfiguration<'a>>,

    pub disable_clean_session: bool,
    pub disable_auto_reconnect: bool,

    pub task_prio: u8,
    pub task_stack: usize,
    pub buffer_size: usize,
    pub out_buffer_size: usize,
    // pub cert_pem: &'a [u8],
    // pub client_cert_pem: &'a [u8],
    // pub client_key_pem: &'a [u8],

    // pub psk_hint_key: KeyHint,
    // pub use_global_ca_store: bool,
    // //esp_err_t (*crt_bundle_attach)(void *conf); /*!< Pointer to ESP x509 Certificate Bundle attach function for the usage of certification bundles in mqtts */
    // pub alpn_protos: &'a [&'a str],

    // pub clientkey_password: &'a str,
    // pub skip_cert_common_name_check: bool,
    // pub use_secure_element: bool,

    // void *ds_data;                          /*!< carrier of handle for digital signature parameters */
}

impl<'a> From<&Configuration<'a>> for (esp_mqtt_client_config_t, RawCstrs) {
    fn from(conf: &Configuration<'a>) -> Self {
        let mut cstrs = RawCstrs::new();

        let c_conf = esp_mqtt_client_config_t {
            client_id: cstrs.as_nptr(conf.client_id),
            // refresh_connection: time::Duration,
            // reconnect_timeout: time::Duration,
            // network_timeout: time::Duration,
            // keepalive: Option<time::Duration>,
            ..Default::default()
        };

        (c_conf, cstrs)
    }
}

struct UnsafeCallback(*mut Box<dyn FnMut(esp_mqtt_event_handle_t)>);

impl UnsafeCallback {
    fn from(boxed: &mut Box<Box<dyn FnMut(esp_mqtt_event_handle_t)>>) -> Self {
        Self(boxed.as_mut())
    }

    unsafe fn from_ptr(ptr: *mut c_types::c_void) -> Self {
        Self(ptr as *mut _)
    }

    fn as_ptr(&self) -> *mut c_types::c_void {
        self.0 as *mut _
    }

    unsafe fn call(&self, data: esp_mqtt_event_handle_t) {
        let reference = self.0.as_mut().unwrap();

        (reference)(data);
    }
}

pub struct EspMqttClient(
    esp_mqtt_client_handle_t,
    Box<dyn FnMut(esp_mqtt_event_handle_t)>,
);

impl EspMqttClient {
    pub fn new<'a>(
        url: impl AsRef<str>,
        conf: &'a Configuration<'a>,
    ) -> Result<(Self, EspMqttConnection), EspError>
    where
        Self: Sized,
    {
        let state = Arc::new(EspMqttConnectionState {
            message: Mutex::new(None),
            posted: Condvar::new(),
            processed: Condvar::new(),
        });

        let connection = EspMqttConnection(state.clone());
        let client_connection = connection.clone();

        let client = Self::new_with_raw_callback(
            url,
            conf,
            Box::new(move |event_handle| EspMqttConnection::post(&client_connection, event_handle)),
        )?;

        Ok((client, connection))
    }

    pub fn new_with_callback<'a>(
        url: impl AsRef<str>,
        conf: &'a Configuration<'a>,
        mut callback: impl for<'b> FnMut(Option<Result<client::Event<EspMqttMessage<'b>>, EspError>>)
            + 'static,
    ) -> Result<Self, EspError>
    where
        Self: Sized,
    {
        Self::new_with_raw_callback(
            url,
            conf,
            Box::new(move |event_handle| {
                let event = unsafe { event_handle.as_ref() };

                if let Some(event) = event {
                    callback(Some(EspMqttMessage::new_event(event, None)))
                } else {
                    callback(None)
                }
            }),
        )
    }

    fn new_with_raw_callback<'a>(
        url: impl AsRef<str>,
        conf: &'a Configuration<'a>,
        raw_callback: Box<dyn FnMut(esp_mqtt_event_handle_t)>,
    ) -> Result<Self, EspError>
    where
        Self: Sized,
    {
        let mut boxed_raw_callback = Box::new(raw_callback);

        let unsafe_callback = UnsafeCallback::from(&mut boxed_raw_callback);

        let (c_conf, _cstrs) = conf.into();

        let client = unsafe { esp_mqtt_client_init(&c_conf as *const _) };
        if client.is_null() {
            esp!(ESP_FAIL)?;
        }

        let client = Self(client, boxed_raw_callback);

        let c_url = CString::new(url.as_ref()).unwrap();

        esp!(unsafe { esp_mqtt_client_set_uri(client.0, c_url.as_ptr()) })?;

        esp!(unsafe {
            esp_mqtt_client_register_event(
                client.0,
                esp_mqtt_event_id_t_MQTT_EVENT_ANY,
                Some(Self::handle),
                unsafe_callback.as_ptr(),
            )
        })?;

        esp!(unsafe { esp_mqtt_client_start(client.0) })?;

        Ok(client)
    }

    extern "C" fn handle(
        event_handler_arg: *mut c_types::c_void,
        _event_base: esp_event_base_t,
        _event_id: i32,
        event_data: *mut c_types::c_void,
    ) {
        unsafe {
            UnsafeCallback::from_ptr(event_handler_arg).call(event_data as _);
        }
    }

    fn check(result: i32) -> Result<client::MessageId, EspError> {
        if result < 0 {
            esp!(result)?;
        }

        Ok(result as _)
    }
}

impl Drop for EspMqttClient {
    fn drop(&mut self) {
        esp!(unsafe { esp_mqtt_client_disconnect(self.0) }).unwrap();
        esp!(unsafe { esp_mqtt_client_stop(self.0) }).unwrap();
        esp!(unsafe { esp_mqtt_client_destroy(self.0) }).unwrap();

        (self.1)(ptr::null_mut() as *mut _);
    }
}

impl service::Service for EspMqttClient {
    type Error = EspError;
}

impl client::Client for EspMqttClient {
    fn subscribe<'a, S>(
        &'a mut self,
        topic: S,
        qos: client::QoS,
    ) -> Result<client::MessageId, Self::Error>
    where
        S: Into<Cow<'a, str>>,
    {
        let c_topic = CString::new(topic.into().as_ref()).unwrap();

        Self::check(unsafe { esp_mqtt_client_subscribe(self.0, c_topic.as_ptr(), qos as _) })
    }

    fn unsubscribe<'a, S>(&'a mut self, topic: S) -> Result<client::MessageId, Self::Error>
    where
        S: Into<Cow<'a, str>>,
    {
        let c_topic = CString::new(topic.into().as_ref()).unwrap();

        Self::check(unsafe { esp_mqtt_client_unsubscribe(self.0, c_topic.as_ptr()) })
    }
}

impl client::Publish for EspMqttClient {
    fn publish<'a, S, V>(
        &'a mut self,
        topic: S,
        qos: client::QoS,
        retain: bool,
        payload: V,
    ) -> Result<client::MessageId, Self::Error>
    where
        S: Into<Cow<'a, str>>,
        V: Into<Cow<'a, [u8]>>,
    {
        let c_topic = CString::new(topic.into().as_ref()).unwrap();

        let payload = payload.into();

        Self::check(unsafe {
            esp_mqtt_client_publish(
                self.0,
                c_topic.as_ptr(),
                payload.as_ref().as_ptr() as _,
                payload.as_ref().len() as _,
                qos as _,
                retain as _,
            )
        })
    }
}

impl client::Enqueue for EspMqttClient {
    fn enqueue<'a, S, V>(
        &'a mut self,
        topic: S,
        qos: client::QoS,
        retain: bool,
        payload: V,
    ) -> Result<client::MessageId, Self::Error>
    where
        S: Into<Cow<'a, str>>,
        V: Into<Cow<'a, [u8]>>,
    {
        let c_topic = CString::new(topic.into().as_ref()).unwrap();

        let payload = payload.into();

        Self::check(unsafe {
            esp_mqtt_client_enqueue(
                self.0,
                c_topic.as_ptr(),
                payload.as_ref().as_ptr() as _,
                payload.as_ref().len() as _,
                qos as _,
                retain as _,
                true,
            )
        })
    }
}

unsafe impl Send for EspMqttClient {}

pub struct EspMqttMessage<'a> {
    event: &'a esp_mqtt_event_t,
    details: client::Details,
    connection: Option<Arc<EspMqttConnectionState>>,
}

impl<'a> EspMqttMessage<'a> {
    #[allow(non_upper_case_globals)]
    fn new_event(
        event: &esp_mqtt_event_t,
        connection: Option<Arc<EspMqttConnectionState>>,
    ) -> Result<client::Event<EspMqttMessage<'_>>, EspError> {
        match event.event_id {
            esp_mqtt_event_id_t_MQTT_EVENT_ERROR => Err(EspError::from(ESP_FAIL).unwrap()), // TODO
            esp_mqtt_event_id_t_MQTT_EVENT_BEFORE_CONNECT => Ok(client::Event::BeforeConnect),
            esp_mqtt_event_id_t_MQTT_EVENT_CONNECTED => {
                Ok(client::Event::Connected(event.session_present != 0))
            }
            esp_mqtt_event_id_t_MQTT_EVENT_DISCONNECTED => Ok(client::Event::Disconnected),
            esp_mqtt_event_id_t_MQTT_EVENT_SUBSCRIBED => {
                Ok(client::Event::Subscribed(event.msg_id as _))
            }
            esp_mqtt_event_id_t_MQTT_EVENT_UNSUBSCRIBED => {
                Ok(client::Event::Unsubscribed(event.msg_id as _))
            }
            esp_mqtt_event_id_t_MQTT_EVENT_PUBLISHED => {
                Ok(client::Event::Published(event.msg_id as _))
            }
            esp_mqtt_event_id_t_MQTT_EVENT_DATA => Ok(client::Event::Received(
                EspMqttMessage::new(event, connection),
            )),
            esp_mqtt_event_id_t_MQTT_EVENT_DELETED => Ok(client::Event::Deleted(event.msg_id as _)),
            other => panic!("Unknown message type: {}", other),
        }
    }

    fn new(event: &'a esp_mqtt_event_t, connection: Option<Arc<EspMqttConnectionState>>) -> Self {
        let mut message = Self {
            event,
            details: client::Details::Complete(unsafe { client::TopicToken::new() }),
            connection,
        };

        message.fill_chunk_details();

        message
    }

    fn fill_chunk_details(&mut self) {
        if self.event.data_len < self.event.total_data_len {
            if self.event.current_data_offset == 0 {
                self.details = client::Details::InitialChunk(client::InitialChunkData {
                    topic_token: unsafe { client::TopicToken::new() },
                    total_data_size: self.event.total_data_len as _,
                });
            } else {
                self.details = client::Details::SubsequentChunk(client::SubsequentChunkData {
                    current_data_offset: self.event.current_data_offset as _,
                    total_data_size: self.event.total_data_len as _,
                });
            }
        }
    }
}

impl<'a> Drop for EspMqttMessage<'a> {
    fn drop(&mut self) {
        if let Some(state) = self.connection.as_ref() {
            let mut message = state.message.lock();

            if message.is_some() {
                *message = None;
                state.processed.notify_all();
            }
        }
    }
}

impl<'a> client::Message for EspMqttMessage<'a> {
    fn id(&self) -> client::MessageId {
        self.event.msg_id as _
    }

    fn data(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(unsafe {
            slice::from_raw_parts(
                (self.event.data as *const u8).as_ref().unwrap(),
                self.event.data_len as _,
            )
        })
    }

    fn topic(&self, _topic_token: &client::TopicToken) -> Cow<'_, str> {
        let ptr = self.event.topic;
        let len = self.event.topic_len;

        unsafe {
            let slice = slice::from_raw_parts(ptr as _, len.try_into().unwrap());
            Cow::Borrowed(core::str::from_utf8(slice).unwrap())
        }
    }

    fn details(&self) -> &client::Details {
        &self.details
    }
}

unsafe impl Send for Newtype<esp_mqtt_event_handle_t> {}

struct EspMqttConnectionState {
    message: Mutex<Option<Newtype<esp_mqtt_event_handle_t>>>,
    posted: Condvar,
    processed: Condvar,
}

#[derive(Clone)]
pub struct EspMqttConnection(Arc<EspMqttConnectionState>);

impl EspMqttConnection {
    fn post(&self, event: esp_mqtt_event_handle_t) {
        let mut message = self.0.message.lock();

        while message.is_some() {
            message = self.0.processed.wait(message);
        }

        *message = Some(Newtype(event));
        self.0.posted.notify_all();
    }
}

unsafe impl Send for EspMqttConnection {}

impl service::Service for EspMqttConnection {
    type Error = EspError;
}

impl client::Connection for EspMqttConnection {
    type Message<'a> = EspMqttMessage<'a>;

    fn next(&mut self) -> Option<Result<client::Event<Self::Message<'_>>, Self::Error>> {
        let mut message = self.0.message.lock();

        while message.is_none() {
            message = self.0.posted.wait(message);
        }

        let event = unsafe { message.as_ref().unwrap().0.as_ref() };
        if let Some(event) = event {
            Some(EspMqttMessage::new_event(event, Some(self.0.clone())))
        } else {
            None
        }
    }
}