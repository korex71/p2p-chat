use anyhow::Result;
use clap::Parser;
use std::{collections::HashMap, fmt, str::FromStr};
use iroh_gossip::{
    net::{Event, Gossip, GossipEvent, GossipReceiver},
    proto::TopicId,
};
use futures_lite::StreamExt;
use iroh::{protocol::Router, Endpoint, NodeAddr, NodeId};
use serde::{Deserialize, Serialize};
use chrono::Local;
use std::sync::{Arc, Mutex};

#[derive(Parser, Debug)]
struct Args {
    #[clap(short, long)]
    name: Option<String>,
    #[clap(short, long, default_value = "0")]
    bind_port: u16,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Parser, Debug)]
enum Command {
    Open,
    Join {
        ticket: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (topic, nodes) = match &args.command {
        Command::Open => {
            let topic = TopicId::from_bytes(rand::random());
            println!("-> opening topic id {topic}");
            (topic, vec![])
        }
        Command::Join { ticket } => {
            let Ticket { topic, nodes } = Ticket::from_str(ticket)?;
            println!("-> joining topic id {topic}");
            (topic, nodes)
        }
    };

    let endpoint = Endpoint::builder()
        .discovery_n0()
        .bind()
        .await?;

    println!("-> node id: {}", endpoint.node_id());
    
    let gossip_builder = Gossip::builder()
        .spawn(endpoint.clone())
        .await?;

    let router_builder = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip_builder.clone())
        .spawn()
        .await?;

    
    let ticket = {
        let me = endpoint.node_addr().await?;
        let nodes = vec![me];
        Ticket { topic, nodes }
    };
    println!("-> ticket to join us: {ticket}");

    let node_ids = nodes.iter().map(|p| p.node_id).collect();
    if nodes.is_empty() {
        println!("-> waiting for nodes to join us");
    } else {
        println!("-> trying to connect to {} nodes", nodes.len());

        for node in nodes.into_iter() {
            endpoint.add_node_addr(node)?;
        }
    };
    
    let (sender, receiver) = gossip_builder.subscribe_and_join(topic, node_ids).await?.split();
    println!("-> connected");

    // broadcast our name, if set
    if let Some(name) = args.name.clone() {
        let message = Message::AboutMe {
            from: endpoint.node_id(),
            name,
        };
        sender.broadcast(message.to_vec().into()).await?;
    }

    let names = Arc::new(Mutex::new(HashMap::new()));
    let names_clone = Arc::clone(&names);

    tokio::spawn(subscribe_loop(receiver, names_clone));

    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel(1);
    
    std::thread::spawn(move || input_loop(line_tx));

    println!("-> send a message to broadcast");

    while let Some(text) = line_rx.recv().await {
        let text = text.trim().to_string();

        if text.starts_with("/") {
            match text.as_str() {
                "/exit" => {
                    println!("leaving chat...");
                    break;
                }
                "/list" => {
                    let names_guard = names.lock().unwrap();

                    println!("user history: {:?}", names_guard.values().collect::<Vec<_>>());
                }
                _ => {
                    println!("unknown command: {}", text);
                }
            }
            continue;
        }

        let message = Message::Message {
            from: endpoint.node_id(),
            text: text.clone(),
        };

        sender.broadcast(message.to_vec().into()).await?;

        let name = args.name.as_ref().map_or_else(|| "you".to_string(), |n| n.clone());
        print_message(&name, &text);
    }

    router_builder.shutdown().await?;

    Ok(())
}

fn print_message(name: &str, text: &str) {
    let timestamp = Local::now().format("%H:%M:%S");
    println!("\x1b[1;32m[{}]\x1b[0m \x1b[1;34m{}:\x1b[0m {}", timestamp, name, text);
}

fn input_loop(line_tx: tokio::sync::mpsc::Sender<String>) -> Result<()> {
    let mut buffer = String::new();
    let cs_stdin = std::io::stdin(); 

    loop {
        cs_stdin.read_line(&mut buffer)?;
        line_tx.blocking_send(buffer.clone())?;
        buffer.clear();
    }
}

async fn subscribe_loop(mut receiver: GossipReceiver,
    names: Arc<Mutex<HashMap<NodeId, String>>>,) -> Result<()> {
    while let Some(event) = receiver.try_next().await? {
        if let Event::Gossip(GossipEvent::Received(msg)) = event {
            match Message::from_bytes(&msg.content)? {
                Message::AboutMe { from, name } => {
                    let mut names_guard = names.lock().unwrap();
                    names_guard.insert(from, name.clone());
                    println!("-> {} joined chat as {}", from.fmt_short(), name);
                    print!("\x07");
                }
                Message::Message { from, text } => {
                    let names_guard = names.lock().unwrap();
                    let name = names_guard
                        .get(&from)
                        .map_or_else(|| from.fmt_short(), String::to_string);

                    print_message(&name, &text);
                    print!("\x07");
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct Ticket {
    topic: TopicId,
    nodes: Vec<NodeAddr>,
}

impl Ticket {
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(Into::into)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("serde_json::to_vec is infallible")
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut text = data_encoding::BASE32_NOPAD.encode(&self.to_bytes()[..]);
        text.make_ascii_lowercase();
        write!(f, "{}", text)
    }
}

impl FromStr for Ticket {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = data_encoding::BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes())?;
        Self::from_bytes(&bytes)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum Message {
    AboutMe { from: NodeId, name: String },
    Message { from: NodeId, text: String },
}

impl Message {
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(Into::into)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("serde_json::to_vec is infallible")
    }
}