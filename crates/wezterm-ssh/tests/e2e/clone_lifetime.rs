use crate::sshd;
use crate::sshd::Sshd;
use assert_fs::fixture::PathChild;
use wezterm_ssh::{Config, Session, SessionEvent};

fn wait_authenticated(events: &smol::channel::Receiver<SessionEvent>) {
    smol::block_on(async {
        loop {
            match events.recv().await {
                Ok(SessionEvent::Authenticated) => break,
                Ok(SessionEvent::HostVerify(verify)) => verify.answer(true).await.unwrap(),
                Ok(SessionEvent::Authenticate(auth)) => {
                    let answers = vec![String::new(); auth.prompts.len()];
                    auth.answer(answers).await.unwrap();
                }
                Ok(_) => {}
                Err(err) => panic!("session events closed during auth: {}", err),
            }
        }
    })
}

#[test]
fn cloned_session_survives_original_drop() {
    if !sshd::sshd_available() {
        return;
    }
    let server = Sshd::spawn(Default::default()).unwrap();
    let mut config = Config::new();
    config.add_config_string("Host localhost\n");
    let mut cfg = config.for_host("localhost");
    cfg.insert("port".to_string(), server.port.to_string());
    cfg.insert("user".to_string(), whoami::username());
    cfg.insert("identitiesonly".to_string(), "yes".to_string());
    cfg.insert(
        "identityfile".to_string(),
        server
            .tmp
            .child("id_rsa")
            .path()
            .to_string_lossy()
            .to_string(),
    );
    cfg.insert(
        "userknownhostsfile".to_string(),
        server
            .tmp
            .child("known_hosts")
            .path()
            .to_string_lossy()
            .to_string(),
    );

    let (session, events) = Session::connect(cfg).unwrap();
    wait_authenticated(&events);

    // Hand the ONLY handle to a "worker" and drop the original here.
    let worker = session.clone();
    drop(session);

    // The clone must keep the session thread alive: canonicalize works,
    // and still works after a pause (the old bug killed the thread on
    // the original handle's drop).
    let sftp = worker.sftp();
    let home = smol::block_on(async { sftp.canonicalize(".").await });
    std::thread::sleep(std::time::Duration::from_secs(1));
    let home2 = smol::block_on(async { sftp.canonicalize(".").await });

    assert!(
        home.is_ok(),
        "first op after original drop failed: {:?}",
        home
    );
    assert!(
        home2.is_ok(),
        "second op after original drop failed: {:?}",
        home2
    );
}
