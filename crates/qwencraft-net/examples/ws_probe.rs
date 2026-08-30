//! Manual WS probe: connect to /ws and print the first frame (Hello).
//! Usage: qwencraft-net-ws-probe ws://127.0.0.1:19123/ws

use std::time::Duration;

fn main() {
    use qwencraft_server::protocol::ClientMsg;
    let url = std::env::args().nth(1).expect("usage: ws-probe <ws-url>");
    let (mut sock, _resp) = expect_or_print(tungstenite::connect(&url));
    // v8 handshake: the identity claim goes out before the server's Hello
    // (all-zero token = fresh identity).
    let _ = sock.send(tungstenite::Message::Binary(
        ClientMsg::Rejoin { token: [0u8; 16] }.encode().into(),
    ));
    match sock.read() {
        Ok(msg) => {
            println!("first frame: {msg:?}");
            if let tungstenite::Message::Binary(data) = msg {
                let (msgs, _) = qwencraft_server::protocol::ServerMsg::decode_stream(&data);
                for m in &msgs {
                    println!("decoded: {m:?}");
                }
            }
        }
        Err(e) => println!("read error: {e:?}"),
    }
    // Keep the socket open a moment so the server can register the player.
    std::thread::sleep(Duration::from_secs(1));
    let _ = sock.close(None);
}

fn expect_or_print<T>(r: Result<T, tungstenite::Error>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            eprintln!("WS PROBE FAILED: {e:?}");
            std::process::exit(1);
        }
    }
}
