mod common;

use tokio::io::AsyncReadExt;
use walgit_git::pkt;
use walgit_git::receive::{self, ReceiveCaps};

mod cm {
    pub use super::common::*;
}

fn encode_data(buf: &mut Vec<u8>, data: &[u8]) {
    pkt::encode_data(buf, data);
}
fn encode_flush(buf: &mut Vec<u8>) {
    pkt::encode_flush(buf);
}

#[tokio::test]
async fn parse_receive_pack_commands_and_push_options() {
    let src = cm::SourceRepo::new();
    let b = src.head();
    let a = cm::SourceRepo::new().head(); // a different commit for the second command
    let pack = src.pack(&["HEAD"], &[], false);
    let zero = "0".repeat(40);

    // Build a receive-pack request body.
    let mut body = Vec::new();
    // First command line carries NUL + capabilities.
    let first =
        format!("{zero} {b} refs/heads/main\0report-status side-band-64k atomic push-options");
    encode_data(&mut body, first.as_bytes());
    // Second command.
    let second = format!("{zero} {a} refs/heads/dev");
    encode_data(&mut body, second.as_bytes());
    encode_flush(&mut body);
    // Push-options section.
    encode_data(&mut body, b"ci=123\n");
    encode_data(&mut body, b"author=alice\n");
    encode_flush(&mut body);
    // Pack bytes follow.
    body.extend_from_slice(&pack);

    let (txn, caps, mut reader) = receive::parse(&body[..]).await.unwrap();
    assert_eq!(txn.updates.len(), 2);
    assert_eq!(txn.updates[0].name, "refs/heads/main");
    assert_eq!(txn.updates[0].new_oid, b);
    assert_eq!(txn.updates[1].name, "refs/heads/dev");
    assert_eq!(txn.updates[1].new_oid, a);
    assert!(caps.report_status);
    assert!(caps.side_band_64k);
    assert!(caps.atomic);
    assert!(caps.push_options);
    assert_eq!(txn.push_options, vec!["ci=123", "author=alice"]);

    // The reader continues at the pack start.
    let mut got = Vec::new();
    reader.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, pack);
    assert!(got.starts_with(b"PACK"));
}

#[tokio::test]
async fn parse_receive_no_commands() {
    let mut body = Vec::new();
    encode_flush(&mut body);
    // No pack follows.
    let (txn, _caps, mut reader) = receive::parse(&body[..]).await.unwrap();
    assert!(txn.updates.is_empty());
    let mut got = Vec::new();
    let _ = reader.read_to_end(&mut got).await;
    assert!(got.is_empty());
}

#[tokio::test]
async fn report_status_sideband_framing() {
    let caps = ReceiveCaps {
        report_status: true,
        report_status_v2: false,
        side_band_64k: true,
        ..Default::default()
    };
    let mut out = Vec::new();
    receive::report_status(
        &caps,
        Ok(()),
        &[("refs/heads/main".to_string(), Ok(()))],
        &mut out,
    )
    .await
    .unwrap();
    // First pkt-line is sideband channel 1 carrying the status block.
    let lines = cm::parse_pkt_lines(&out);
    let last = lines.last().unwrap();
    assert!(matches!(last, cm::PktLine::Flush));
    let data = lines.iter().find_map(|l| match l {
        cm::PktLine::Data(b) => Some(b.clone()),
        _ => None,
    });
    let data = data.expect("a data pkt-line");
    assert_eq!(data[0], 1); // channel 1
    let block = String::from_utf8_lossy(&data[1..]);
    assert!(block.contains("unpack ok"));
    assert!(block.contains("ok refs/heads/main"));
}

#[tokio::test]
async fn report_status_plain_v2_ng() {
    let caps = ReceiveCaps {
        report_status: true,
        report_status_v2: true,
        side_band_64k: false,
        ..Default::default()
    };
    let mut out = Vec::new();
    receive::report_status(
        &caps,
        Ok(()),
        &[(
            "refs/heads/x".to_string(),
            Err("non-fast-forward".to_string()),
        )],
        &mut out,
    )
    .await
    .unwrap();
    let s = String::from_utf8(out).unwrap();
    // v0 and v2: one line `ng <ref> <reason>`
    assert!(s.contains("unpack ok"));
    assert!(s.contains("ng refs/heads/x non-fast-forward"));
    assert!(s.contains("0000")); // final flush
}

/// report-status-v2 permits `option …` only after `ok <ref>`. A rejected *atomic* transaction is
/// `ng <ref> <reason>` per command and nothing more: an `option atomic` line after the `ng`s made
/// every losing pusher in a race see "'option' without a matching 'ok/ng' directive" instead of
/// the reason.
#[tokio::test]
async fn report_status_v2_atomic_rejection_has_no_option_line() {
    let caps = ReceiveCaps {
        report_status: true,
        report_status_v2: true,
        atomic: true,
        ..Default::default()
    };
    let mut out = Vec::new();
    receive::report_status(
        &caps,
        Ok(()),
        &[
            (
                "refs/heads/main".to_string(),
                Err("fetch first".to_string()),
            ),
            (
                "refs/heads/other".to_string(),
                Err("atomic transaction failed".to_string()),
            ),
        ],
        &mut out,
    )
    .await
    .unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("ng refs/heads/main fetch first"), "{s}");
    assert!(
        s.contains("ng refs/heads/other atomic transaction failed"),
        "{s}"
    );
    assert!(
        !s.contains("option"),
        "no option line may follow an ng: {s}"
    );
}

/// A shallow client (push from a `--depth` clone) announces `shallow <oid>`
/// lines before its commands: they are recorded, not mistaken for commands
/// (prod: "protocol error: missing ref name" → 500).
#[tokio::test]
async fn parse_receive_accepts_shallow_lines() {
    let src = cm::SourceRepo::new();
    let b = src.head();
    let zero = "0".repeat(40);
    let mut body = Vec::new();
    encode_data(&mut body, format!("shallow {b}\n").as_bytes());
    encode_data(&mut body, format!("shallow {zero}\n").as_bytes());
    encode_data(
        &mut body,
        format!("{zero} {b} refs/heads/main\0report-status side-band-64k").as_bytes(),
    );
    encode_flush(&mut body);
    let (txn, caps, _rest) = receive::parse(std::io::Cursor::new(body)).await.unwrap();
    assert_eq!(txn.updates.len(), 1);
    assert_eq!(txn.updates[0].name, "refs/heads/main");
    assert!(caps.side_band_64k);
    assert_eq!(caps.shallow, vec![b.clone(), zero]);
}
