use std::collections::VecDeque;
use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use mqtt3::{self, LastWill, Packet, PacketIdentifier};

use data::{Client, ClientCallback};
use fnv::FnvHashMap;
use net::api::{Socket, Stream};
use net::timer::{NetTimers, TimerCallback};
use util;

use atom::Atom;

pub struct ClientNodeImpl {
    socket: Option<Socket>,
    stream: Option<Stream>,

    connect_func: Option<ClientCallback>,
    close_func: Option<ClientCallback>,

    curr_sub_id: u16,
    curr_unsub_id: u16,
    // 奇数表示sub，偶数表示unsub
    sub_map: FnvHashMap<usize, Option<ClientCallback>>,

    attributes: FnvHashMap<Atom, Arc<Vec<u8>>>,

    // topics由set_topic_handler设置回调
    topics: FnvHashMap<Atom, TopicData>,
    topic_patterns: FnvHashMap<Atom, TopicData>,

    // 当socket和stream还没准备好时候的缓冲区
    socket_handlers: VecDeque<Box<FnOnce(&Socket, Stream)>>,
    keep_alive: u16,
}

#[derive(Clone)]
pub struct ClientNode(pub Arc<Mutex<ClientNodeImpl>>);

unsafe impl Sync for ClientNodeImpl {}
unsafe impl Send for ClientNodeImpl {}

struct TopicData {
    topic: mqtt3::TopicPath,
    func: Box<Fn(Result<(Socket, &[u8])>)>,
}

impl ClientNode {
    pub fn new() -> Self {
        ClientNode(Arc::new(Mutex::new(ClientNodeImpl {
            socket: None,
            stream: None,

            connect_func: None,
            close_func: None,

            attributes: FnvHashMap::default(),

            curr_sub_id: 0,
            curr_unsub_id: 0,
            sub_map: FnvHashMap::default(),

            topics: FnvHashMap::default(),
            topic_patterns: FnvHashMap::default(),
            socket_handlers: VecDeque::new(),
            keep_alive: 0,
        })))
    }
    pub fn get_socket(&self) -> Socket {
        let node = self.0.lock().unwrap();
        node.socket.clone().unwrap().clone()
    }

    //只有在keep_alive时间内都没有数据包发送才会发送ping包
    pub fn ping(&self) {
        let client = self.clone();
        let keep_alive;
        let socket;
        {
            let node = self.0.lock().unwrap();
            keep_alive = node.keep_alive;
            socket = node.socket.clone();
        }
        if keep_alive > 0 {
            let timers = self.get_timers();
            let mut timers = timers.write().unwrap();
            timers.set_timeout(
                Atom::from(String::from("client_ping")),
                Duration::from_secs(keep_alive as u64),
                Box::new(move |_src: Atom| {
                    println!("keep_alive timeout ping !!!!!!!!!!!!");
                    let socket = socket.unwrap();
                    //发送数据
                    util::send_pingreq(&socket);
                    //递归
                    client.ping();
                }),
            )
        }
    }
    //获取net定时器
    pub fn get_timers(&self) -> Arc<RwLock<NetTimers<TimerCallback>>> {
        let node = self.0.lock().unwrap();
        let stream = node.stream.clone().unwrap();
        match stream {
            Stream::Raw(s) => s.read().unwrap().net_timers.clone(),
            Stream::Tls(s) => s.read().unwrap().get_timers(),
        }
    }
}

impl Client for ClientNode {
    fn set_stream(&self, socket: Socket, stream: Stream) {
        let node = &mut self.0.lock().unwrap();

        while !node.socket_handlers.is_empty() {
            let func = node.socket_handlers.pop_front().unwrap();
            func(&socket, stream.clone());
        }

        node.socket = Some(socket);
        node.stream = Some(stream);
    }

    fn connect(
        &self,
        keep_alive: u16,
        will: Option<LastWill>,
        close_func: Option<ClientCallback>,
        connect_func: Option<ClientCallback>,
    ) {
        {
            let node = &mut self.0.lock().unwrap();
            node.close_func = close_func;
            node.connect_func = connect_func;
            node.keep_alive = keep_alive;
        }

        let node = self.0.clone();
        let func = Box::new(move |socket: &Socket, stream: Stream| {
            handle_connect(node, socket, stream, keep_alive, will);
        });
        handle_slot(self.0.clone(), func);
    }

    fn subscribe(
        &self,
        topics: Vec<(String, mqtt3::QoS)>,
        resp_func: Option<ClientCallback>,
    ) -> Result<()> {
        let curr_id;
        {
            let node = &mut self.0.lock().unwrap();

            // 检查参数合法性
            let mut ts = Vec::with_capacity(topics.len());
            for &(ref name, ref _qos) in topics.iter() {
                let map;
                if is_topic_contains_wildcards(name)? {
                    map = &node.topic_patterns;
                } else {
                    map = &node.topics;
                }

                if map.contains_key(&Atom::from(name.clone())) {
                    ts.push((name.to_string(), mqtt3::QoS::AtMostOnce));
                } else {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("Client Subscribe, topic {} can't find handler!", name),
                    ));
                }
            }

            curr_id = node.curr_sub_id;
            node.sub_map.insert((2 * curr_id + 1) as usize, resp_func);
            if node.curr_sub_id < u16::max_value() {
                node.curr_sub_id += 1;
            } else {
                node.curr_sub_id = 0;
            }
        }

        let func = Box::new(move |socket: &Socket, _stream: Stream| {
            util::send_subscribe(socket, curr_id, topics);
        });
        handle_slot(self.0.clone(), func);

        return Ok(());
    }

    fn unsubscribe(
        &self,
        topics: Vec<String>,
        resp_func: Option<ClientCallback>,
    ) -> Result<()> {
        let curr_id;
        {
            let node = &mut self.0.lock().unwrap();
            // 检查参数合法性
            let mut ts = Vec::with_capacity(topics.len());
            for name in topics.iter() {
                let map;
                if is_topic_contains_wildcards(name)? {
                    map = &node.topic_patterns;
                } else {
                    map = &node.topics;
                }

                if map.contains_key(&Atom::from(name.clone())) {
                    ts.push((name.to_string(), mqtt3::QoS::AtMostOnce));
                } else {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("Client Subscribe, topic {} can't find handler!", name),
                    ));
                }
            }

            curr_id = node.curr_unsub_id;
            node.sub_map.insert((2 * curr_id) as usize, resp_func);
            if node.curr_unsub_id < u16::max_value() {
                node.curr_unsub_id += 1;
            } else {
                node.curr_unsub_id = 0;
            }
        }

        let func = Box::new(move |socket: &Socket, _stream: Stream| {
            util::send_unsubscribe(socket, curr_id, topics);
        });
        handle_slot(self.0.clone(), func);

        return Ok(());
    }

    fn disconnect(&self) -> Result<()> {
        let func = Box::new(move |socket: &Socket, _stream: Stream| {
            util::send_disconnect(socket);
        });
        handle_slot(self.0.clone(), func);
        let node = &mut self.0.lock().unwrap();

        // 删除所有的数据结构
        node.connect_func = None;
        node.close_func = None;
        node.curr_sub_id = 0;
        node.curr_unsub_id = 0;
        node.sub_map.clear();
        node.attributes.clear();
        node.topics.clear();
        node.topic_patterns.clear();
        node.socket_handlers.clear();
        return Ok(());
    }

    fn publish(
        &self,
        retain: bool,
        _qos: mqtt3::QoS,
        topic: Atom,
        payload: Vec<u8>,
    ) -> Result<()> {
        if is_topic_contains_wildcards(&topic)? {
            return Err(Error::new(ErrorKind::Other, "InvalidPublishTopic"));
        }

        let func = Box::new(move |socket: &Socket, _stream: Stream| {
            let topic = topic.to_string();
            util::send_publish(socket, retain, mqtt3::QoS::AtMostOnce, &topic, payload);
        });
        handle_slot(self.0.clone(), func);

        return Ok(());
    }

    fn set_topic_handler(
        &self,
        name: Atom,
        handler: Box<Fn(Result<(Socket, &[u8])>)>,
    ) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        let topic;
        match mqtt3::TopicPath::from_str((*name).clone().as_str()) {
            Ok(t) => topic = t,
            Err(_) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("InvalidTopic, {}", *name),
                ))
            }
        }

        let map;
        if topic.wildcards {
            map = &mut node.topic_patterns;
        } else {
            map = &mut node.topics;
        }

        map.insert(
            name,
            TopicData {
                topic,
                func: handler,
            },
        );
        return Ok(());
    }

    fn remove_topic_handler(&self, name: Atom) -> Result<()> {
        let node = &mut self.0.lock().unwrap();
        let topic;
        match mqtt3::TopicPath::from_str((*name).clone().as_str()) {
            Ok(t) => topic = t,
            Err(_) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("InvalidTopic, {}", *name),
                ))
            }
        }

        let map;
        if topic.wildcards {
            map = &mut node.topic_patterns;
        } else {
            map = &mut node.topics;
        }

        map.remove(&name);
        return Ok(());
    }

    fn add_attribute(&self, name: Atom, value: Vec<u8>) {
        let node = &mut self.0.lock().unwrap();
        let has_attr = node.attributes.contains_key(&name);
        if !has_attr {
            node.attributes.insert(name, Arc::new(value));
        }
    }

    fn remove_attribute(&self, name: Atom) {
        let node = &mut self.0.lock().unwrap();
        node.attributes.remove(&name);
    }

    fn get_attribute(&self, name: Atom) -> Option<Arc<Vec<u8>>> {
        let node = &mut self.0.lock().unwrap();
        return match node.attributes.get(&name) {
            None => None,
            Some(v) => Some(v.clone()),
        };
    }
}

fn handle_connect(
    node: Arc<Mutex<ClientNodeImpl>>,
    socket: &Socket,
    stream: Stream,
    keep_alive: u16,
    last_will: Option<LastWill>,
) {
    util::send_connect(socket, keep_alive, last_will);

    let s = stream.clone();
    util::recv_mqtt_packet(
        stream,
        Box::new(move |packet: Result<Packet>| {
            handle_recv(node.clone(), s.clone(), packet);
        }),
    );
}

fn handle_recv(
    node: Arc<Mutex<ClientNodeImpl>>,
    stream: Stream,
    packet: Result<Packet>,
) {
    let n = node.clone();
    if let Ok(packet) = packet {
        match packet {
            Packet::Connack(ack) => recv_connect_ack(n, ack),
            Packet::Suback(ack) => recv_sub_ack(n, ack),
            Packet::Unsuback(PacketIdentifier(id)) => recv_unsub_ack(n, id),
            Packet::Publish(publish) => recv_publish(n, publish),
            Packet::Pingresp => recv_pingresp(n),
            _ => panic!("client handle_recv: invalid packet!"),
        }
    }

    {
        let s = stream.clone();
        let n = node.clone();
        util::recv_mqtt_packet(
            stream,
            Box::new(move |packet: Result<Packet>| {
                handle_recv(n.clone(), s.clone(), packet);
            }),
        );
    }
}

fn recv_pingresp(_node: Arc<Mutex<ClientNodeImpl>>) {
    // TODO: impl
}

fn recv_connect_ack(node: Arc<Mutex<ClientNodeImpl>>, ack: mqtt3::Connack) {
    use mqtt3::ConnectReturnCode;
    let r = match ack.code {
        ConnectReturnCode::Accepted => Ok(()),
        ConnectReturnCode::RefusedProtocolVersion => Err(Error::new(
            ErrorKind::Other,
            "Packet::Connack, RefusedProtocolVersion",
        )),
        ConnectReturnCode::RefusedIdentifierRejected => Err(Error::new(
            ErrorKind::Other,
            "Packet::Connack, RefusedIdentifierRejected",
        )),
        ConnectReturnCode::ServerUnavailable => Err(Error::new(
            ErrorKind::Other,
            "Packet::Connack, ServerUnavailable",
        )),
        ConnectReturnCode::BadUsernamePassword => Err(Error::new(
            ErrorKind::Other,
            "Packet::Connack, BadUsernamePassword",
        )),
        ConnectReturnCode::NotAuthorized => Err(Error::new(
            ErrorKind::Other,
            "Packet::Connack, NotAuthorized",
        )),
    };

    if let Some(func) = node.lock().unwrap().connect_func.take() {
        func(r);
    }
}

fn recv_sub_ack(node: Arc<Mutex<ClientNodeImpl>>, ack: mqtt3::Suback) {
    let node = &mut node.lock().unwrap();
    let PacketIdentifier(id) = ack.pid;
    let id = (1 + id * 2) as usize;
    if let Some(Some(func)) = node.sub_map.remove(&id) {
        func(Ok(()));
    }
}

fn recv_unsub_ack(node: Arc<Mutex<ClientNodeImpl>>, id: u16) {
    let node = &mut node.lock().unwrap();
    let id = (id * 2) as usize;
    if let Some(Some(func)) = node.sub_map.remove(&id) {
        func(Ok(()));
    }
}

fn recv_publish(node: Arc<Mutex<ClientNodeImpl>>, publish: mqtt3::Publish) {
    let node = &mut node.lock().unwrap();

    let publish_topic = mqtt3::TopicPath::from_str(&publish.topic_name);
    if let Err(_) = publish_topic {
        return;
    }

    let atom = Atom::from(publish.topic_name.as_str());
    let socket = node.socket.clone().unwrap();
    if let Some(data) = node.topics.get(&atom) {
        (data.func)(Ok((socket.clone(), publish.payload.as_slice())));
    }
    let publish_topic = publish_topic.unwrap();
    for (_, data) in node.topic_patterns.iter() {
        if data.topic.is_match(&publish_topic) {
            (data.func)(Ok((socket.clone(), publish.payload.as_slice())));
        }
    }
}

fn handle_slot(node: Arc<Mutex<ClientNodeImpl>>, func: Box<FnOnce(&Socket, Stream)>) {
    let node = node.clone();
    {
        
        let node = &mut node.lock().unwrap();
        let no_socket = node.socket.is_none();

        if no_socket {
            node.socket_handlers.push_back(func);
            return;
        }

        if let Some(ref socket) = node.socket.as_ref() {
            let stream = node.stream.as_ref().unwrap();
            func(socket, stream.clone());
        }
    }
    let client = ClientNode(node.clone());
    //只有在keep_alive时间内都没有数据包发送才会发送ping包
    client.ping();
}

fn is_topic_contains_wildcards(name: &str) -> Result<bool> {
    return match mqtt3::TopicPath::from_str(name) {
        Ok(topic) => Ok(topic.wildcards),
        Err(_e) => Err(Error::new(
            ErrorKind::Other,
            format!("InvalidTopic, {}", name),
        )),
    };
}
