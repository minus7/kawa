use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;
use std::io::Read;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::mem;
use hyper::client::Client;
use rustc_serialize::json;

use shout;
use queue::{Queue, QueueEntry};
use api::{ApiMessage, QueuePos};
use config::{Config, StreamConfig, ApiConfig};
use transcode;
use ring_buffer::RingBuffer;

struct PreBuffer {
    buffer: Arc<RingBuffer<u8>>,
    token: Arc<AtomicBool>,
}

impl PreBuffer {
    fn from_transcode(input: Arc<Vec<u8>>, ext: &str, cfg: &StreamConfig) -> Option<PreBuffer> {
        let token = Arc::new(AtomicBool::new(false));
        // 500KB Buffer
        let out_buf = Arc::new(RingBuffer::new(500000));
        let res_ext = match cfg.container {
            shout::ShoutFormat::Ogg => "ogg",
            shout::ShoutFormat::MP3 => "mp3",
            _ => {
                return None;
            }
        };
        if let Err(e) = transcode::transcode(input,
                                             ext,
                                             out_buf.clone(),
                                             res_ext,
                                             cfg.codec,
                                             cfg.bitrate,
                                             token.clone()) {
            println!("WARNING: Transcoder creation failed with error: {}", e);
            None
        } else {
            Some(PreBuffer {
                buffer: out_buf,
                token: token,
            })
        }
    }

    fn cancel(self) {
        self.token.store(true, Ordering::SeqCst);
        loop {
            if self.buffer.len() > 0 {
                self.buffer.try_read(4096);
            } else {
                break;
            }
        }
    }
}

struct RadioConn {
    tx: Sender<Arc<RingBuffer<u8>>>,
    handle: thread::JoinHandle<()>,
}

impl RadioConn {
    fn new(host: String,
           port: u16,
           user: String,
           password: String,
           mount: String,
           format: shout::ShoutFormat) -> RadioConn {
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            let conn = shout::ShoutConnBuilder::new()
                .host(host)
                .port(port)
                .user(user)
                .password(password)
                .mount(mount)
                .protocol(shout::ShoutProtocol::HTTP)
                .format(format)
                .build()
                .unwrap();
            play(conn, rx);
        });
        RadioConn {
            tx: tx,
            handle: handle,
        }
    }

    fn replace_buffer(&mut self, buffer: Arc<RingBuffer<u8>>) {
        self.tx.send(buffer).unwrap();
    }
}

fn get_random_song(cfg: &ApiConfig) -> QueueEntry {
    let client = Client::new();

    // TODO: Handle failure
    let mut res = client.get(cfg.remote_random.clone()).send().unwrap();
    let mut body = String::new();
    res.read_to_string(&mut body).unwrap();
    return json::decode(&body).unwrap()
}

pub fn play(conn: shout::ShoutConn, buffer_rec: Receiver<Arc<RingBuffer<u8>>>) {
    let step = 4096;
    let mut buffer = buffer_rec.recv().unwrap();
    loop {
        match buffer_rec.try_recv() {
            Ok(b) => { buffer = b; }
            Err(TryRecvError::Empty) => { }
            Err(TryRecvError::Disconnected) => { return }
        }

        if buffer.len() > 0 {
            conn.send(buffer.try_read(step));
            conn.sync();
        } else {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn initiate_transcode(path: String, stream_cfgs: &Vec<StreamConfig>) -> Option<Vec<PreBuffer>> {
    let mut in_buf = Vec::new();
    let mut prebufs = Vec::new();

    let ext = match Path::new(&path).extension() {
        Some(e) => e,
        None => return None,
    };

    if let None = File::open(&path).ok().and_then(|mut f| f.read_to_end(&mut in_buf).ok()) {
        return None;
    }
    let in_buf = Arc::new(in_buf);

    for stream in stream_cfgs.iter() {
        if let Some(prebuf) = PreBuffer::from_transcode(in_buf.clone(),
                                                        ext.to_str().unwrap(),
                                                        stream) {
            prebufs.push(prebuf);
        }
    }
    Some(prebufs)
}

fn get_queue_prebuf(queue: Arc<Mutex<Queue>>,
                    configs: &Vec<StreamConfig>)
                    -> Option<Vec<PreBuffer>> {
    let mut queue = queue.lock().unwrap();
    while !queue.entries.is_empty() {
        if let Some(r) = initiate_transcode(queue.entries[0].path.clone(), configs) {
            return Some(r);
        } else {
            queue.entries.pop();
        }
    }
    None
}

fn get_random_prebuf(cfg: &Config) -> Vec<PreBuffer> {
    let mut counter = 0;
    loop {
        if counter == 100 {
            panic!("Your random shit is broken.");
        }
        let random = get_random_song(&cfg.api);
        if let Some(p) = initiate_transcode(random.path.clone(), &cfg.streams) {
            return p;
        }
        counter += 1;
    }
}

pub fn start_streams(cfg: Config,
                     queue: Arc<Mutex<Queue>>,
                     updates: Receiver<ApiMessage>) {
    let mut random_prebuf = get_random_prebuf(&cfg);
    let mut queue_prebuf = get_queue_prebuf(queue.clone(), &cfg.streams);
    let mut rconns: Vec<_> = cfg.streams.iter()
        .map(|stream| {
            RadioConn::new(cfg.radio.host.clone(),
                             cfg.radio.port,
                             cfg.radio.user.clone(),
                             cfg.radio.password.clone(),
                             stream.mount.clone(),
                             stream.container.clone())
        })
        .collect();
    loop {
        // Get prebuffers for next up song, using random if nothing's in the queue
        let prebuffers = if queue_prebuf.is_some() {
            queue.lock().unwrap().entries.remove(0);
            mem::replace(&mut queue_prebuf,
                         get_queue_prebuf(queue.clone(), &cfg.streams))
                .unwrap()
        } else {
            mem::replace(&mut random_prebuf, get_random_prebuf(&cfg))
        };

        for (rconn, pb) in rconns.iter_mut().zip(prebuffers.iter()) {
            rconn.replace_buffer(pb.buffer.clone());
        }

        // Song activity loop - ensures that the song is properly transcoding and handles any sort
        // of API message that gets received in the meanwhile
        loop {
            // If the prebuffers are all completed, complete loop iteration, requeue next song
            if prebuffers.iter()
                .all(|prebuffer| {
                    prebuffer.token.load(Ordering::Acquire) && prebuffer.buffer.len() == 0
                }) {
                break;
            } else {
                if let Ok(msg) = updates.try_recv() {
                    match msg {
                        ApiMessage::Skip => {
                            for prebuffer in prebuffers.iter() {
                                prebuffer.token.store(true, Ordering::Release);
                            }
                            break;
                        }
                        ApiMessage::Clear => {
                            if queue_prebuf.is_some() {
                                for prebuf in mem::replace(&mut queue_prebuf, None).unwrap() {
                                    prebuf.cancel();
                                }
                            }
                            queue.lock().unwrap().clear();
                        }
                        ApiMessage::Insert(QueuePos::Head, qe) => {
                            {
                                let mut q = queue.lock().unwrap();
                                q.insert(0, qe);
                            }
                            let old_prebufs = mem::replace(&mut queue_prebuf,
                                                           get_queue_prebuf(queue.clone(),
                                                                            &cfg.streams))
                                .unwrap();
                            for prebuf in old_prebufs {
                                prebuf.cancel();
                            }
                        }
                        ApiMessage::Insert(QueuePos::Tail, qe) => {
                            let mut q = queue.lock().unwrap();
                            q.push(qe);
                            if q.len() == 1 {
                                drop(q);
                                queue_prebuf = get_queue_prebuf(queue.clone(), &cfg.streams);
                            }
                        }
                        ApiMessage::Remove(QueuePos::Head) => {
                            let mut q = queue.lock().unwrap();
                            if q.len() > 0 {
                                q.remove(0);
                                drop(q);
                                let old_prebufs = mem::replace(&mut queue_prebuf,
                                                               get_queue_prebuf(queue.clone(),
                                                                                &cfg.streams))
                                    .unwrap();
                                for prebuf in old_prebufs {
                                    prebuf.cancel();
                                }
                            }
                        }
                        ApiMessage::Remove(QueuePos::Tail) => {
                            let mut q = queue.lock().unwrap();
                            if q.len() > 0 {
                                q.pop();
                            }
                            if q.len() == 0 {
                                drop(q);
                                if let Some(old_prebufs) = mem::replace(&mut queue_prebuf, None) {
                                    for prebuf in old_prebufs {
                                        prebuf.cancel();
                                    }
                                }
                            }
                        }
                    }
                } else {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}
