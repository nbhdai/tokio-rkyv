use futures::prelude::*;
use tokio::net::TcpStream;
use tokio_util::codec::{FramedWrite, LengthDelimitedCodec};

#[derive(Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Hello {
    name: String,
}

#[tokio::main]
pub async fn main() {
    // Bind a server socket
    let socket = TcpStream::connect("127.0.0.1:17653").await.unwrap();

    // Delimit frames using a length header
    let length_delimited = FramedWrite::new(socket, LengthDelimitedCodec::new());

    // Serialize frames with JSON
    let mut serialized =
        tokio_rkyv::SymmetricallyFramed::new(length_delimited);

    // Send the value
    serialized
        .send(Hello {
            name: "world".to_string(),
        })
        .await
        .unwrap()
}
