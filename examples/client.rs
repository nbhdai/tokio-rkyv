use futures::prelude::*;
use tokio::net::TcpStream;
use tokio_rkyv::RkyvSink;

#[derive(Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Hello {
    name: String,
}

#[tokio::main]
pub async fn main() {
    // Bind a server socket
    let socket = TcpStream::connect("127.0.0.1:17653").await.unwrap();

    // Delimit frames using a length header
    let mut serialized = RkyvSink::new(socket);

    // Send the value
    serialized
        .send(Hello {
            name: "world".to_string(),
        })
        .await
        .unwrap()
}
