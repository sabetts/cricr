//! End-to-end test: drive the client against a scripted in-process server.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::Instant;

use cricr_irc::sasl::SaslCredentials;
use cricr_irc::{spawn, ClientCommand, ClientConfig, Event};

async fn read_line<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Option<String> {
    let mut buf = String::new();
    match reader.read_line(&mut buf).await {
        Ok(0) => None,
        Ok(_) => Some(buf.trim_end().to_string()),
        Err(_) => None,
    }
}

#[tokio::test]
async fn registers_authenticates_and_chats() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut wr) = stream.into_split();
        let mut rd = BufReader::new(read_half);
        let mut saw_user = false;

        while let Some(line) = read_line(&mut rd).await {
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("CAP LS") {
                wr.write_all(b":srv CAP * LS :sasl server-time message-tags\r\n")
                    .await
                    .unwrap();
            } else if upper.starts_with("CAP REQ") {
                let caps = line.split_once(':').map(|x| x.1).unwrap_or("");
                wr.write_all(format!(":srv CAP * ACK :{caps}\r\n").as_bytes())
                    .await
                    .unwrap();
            } else if upper.starts_with("AUTHENTICATE PLAIN") {
                wr.write_all(b"AUTHENTICATE +\r\n").await.unwrap();
            } else if upper.starts_with("AUTHENTICATE ") {
                // The base64 client response.
                wr.write_all(b":srv 900 nick nick!u@h :You are now logged in\r\n")
                    .await
                    .unwrap();
                wr.write_all(b":srv 903 nick :SASL authentication successful\r\n")
                    .await
                    .unwrap();
            } else if upper.starts_with("CAP END") {
                wr.write_all(b":srv 001 nick :Welcome to the test server\r\n")
                    .await
                    .unwrap();
                wr.write_all(b":srv 005 nick NICKLEN=30 :are supported\r\n")
                    .await
                    .unwrap();
            } else if upper.starts_with("USER ") {
                saw_user = true;
            } else if upper.starts_with("JOIN #TEST") {
                wr.write_all(b":nick!u@h JOIN #test\r\n").await.unwrap();
                wr.write_all(b":srv 353 nick = #test :@nick other\r\n")
                    .await
                    .unwrap();
                wr.write_all(b":srv 366 nick #test :End of /NAMES list\r\n")
                    .await
                    .unwrap();
                wr.write_all(b":other!u@h PRIVMSG #test :hi there\r\n")
                    .await
                    .unwrap();
            }
        }
        assert!(saw_user, "client never sent USER");
    });

    let (handle, mut rx) = spawn(ClientConfig {
        server: "127.0.0.1".to_string(),
        port: addr.port(),
        tls: false,
        nick: "nick".to_string(),
        user_name: "u".to_string(),
        full_name: "n".to_string(),
        server_password: None,
        sasl: Some(SaslCredentials::plain("nick", "pw")),
        client_cert: None,
        channels: vec!["#test".to_string()],
    });

    let mut logged_in = false;
    let mut registered = false;
    let mut joined = false;
    let mut privmsg = false;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !(logged_in && registered && joined && privmsg) {
        if let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            match event {
                Event::LoggedIn => logged_in = true,
                Event::Registered { .. } => registered = true,
                Event::Join { .. } => joined = true,
                Event::Privmsg { text, .. } if text == "hi there" => privmsg = true,
                _ => {}
            }
        }
    }

    assert!(logged_in, "SASL did not complete");
    assert!(registered, "never received 001");
    assert!(joined, "never joined the channel");
    assert!(privmsg, "never received the PRIVMSG");

    handle.send(ClientCommand::Disconnect);
    let _ = server.await;
}
