//! Bug A repro: answer a Chrome-shaped recvonly video offer with `MediaEgress` and inspect
//! the SDP we would relay to the browser. The phone showed `setRemoteDescription failed
//! (DOMException)` — the prime suspect is datachannel-rs's lossy SDP round-trip
//! (`rtcGetLocalDescription` → `webrtc_sdp::parse_sdp` → `Display`), which leaner parsers
//! (ffmpeg WHIP, libdatachannel loopback) tolerate but Chrome rejects. This reproduces the
//! answer generation on the laptop, prints it, and asserts the attributes Chrome requires.
#![cfg(feature = "server")]

use whip_ingest::MediaEgress;

/// A realistic Chrome ~126 recvonly-video offer (one m=video transceiver, BUNDLE,
/// non-trickle: candidates + end-of-candidates included). Values are synthetic but
/// well-formed; libdatachannel only needs to parse them to produce its answer.
const CHROME_OFFER: &str = concat!(
    "v=0\r\n",
    "o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n",
    "s=-\r\n",
    "t=0 0\r\n",
    "a=group:BUNDLE 0\r\n",
    "a=extmap-allow-mixed\r\n",
    "a=msid-semantic: WMS\r\n",
    "m=video 51234 UDP/TLS/RTP/SAVPF 96 97 102 103 104 105\r\n",
    "c=IN IP4 192.168.1.166\r\n",
    "a=rtcp:9 IN IP4 0.0.0.0\r\n",
    "a=candidate:1 1 udp 2113937151 192.168.1.166 51234 typ host generation 0 network-cost 999\r\n",
    "a=end-of-candidates\r\n",
    "a=ice-ufrag:oVXe\r\n",
    "a=ice-pwd:B2vJvBSoe3rC3TdUJEmOAAAA\r\n",
    "a=ice-options:trickle\r\n",
    "a=fingerprint:sha-256 39:3A:56:9C:1E:60:4F:24:6C:81:D9:0B:6C:87:34:41:E2:16:B6:D8:A0:C6:63:52:CF:07:5B:9B:B4:14:63:88\r\n",
    "a=setup:actpass\r\n",
    "a=mid:0\r\n",
    "a=extmap:1 urn:ietf:params:rtp-hdrext:toffset\r\n",
    "a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n",
    "a=recvonly\r\n",
    "a=rtcp-mux\r\n",
    "a=rtcp-rsize\r\n",
    "a=rtpmap:96 VP8/90000\r\n",
    "a=rtcp-fb:96 nack\r\n",
    "a=rtpmap:97 rtx/90000\r\n",
    "a=fmtp:97 apt=96\r\n",
    "a=rtpmap:102 H264/90000\r\n",
    "a=rtcp-fb:102 goog-remb\r\n",
    "a=rtcp-fb:102 nack\r\n",
    "a=rtcp-fb:102 nack pli\r\n",
    "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n",
    "a=rtpmap:103 rtx/90000\r\n",
    "a=fmtp:103 apt=102\r\n",
    "a=rtpmap:104 H264/90000\r\n",
    "a=fmtp:104 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r\n",
    "a=rtpmap:105 rtx/90000\r\n",
    "a=fmtp:105 apt=104\r\n",
);

#[test]
fn answer_to_a_chrome_offer_keeps_what_chrome_requires() {
    std::env::set_var("UNSTATION_BIND_ADDR", "127.0.0.1");
    let egress = MediaEgress::answer(CHROME_OFFER, &[]).expect("egress answers the offer");
    let sdp = egress.answer_sdp();
    println!("=== ANSWER SDP the browser would receive ===\n{sdp}\n=== END ===");

    // Line-level requirements for Chrome's setRemoteDescription(answer):
    let has = |needle: &str| sdp.lines().any(|l| l.starts_with(needle));
    assert!(has("v=0"), "version line");
    assert!(has("o="), "origin line");
    assert!(has("m=video"), "video m-line");
    assert!(has("c=IN IP4"), "connection line (session or media level)");
    assert!(has("a=ice-ufrag:"), "ice-ufrag");
    assert!(has("a=ice-pwd:"), "ice-pwd");
    assert!(has("a=fingerprint:sha-256"), "DTLS fingerprint");
    assert!(
        has("a=setup:active") || has("a=setup:passive"),
        "answer must pick a DTLS role (offer was actpass)"
    );
    assert!(has("a=mid:0"), "mid must echo the offer's");
    assert!(has("a=sendonly"), "answer direction opposite of recvonly");
    assert!(has("a=rtcp-mux"), "Chrome offered rtcp-mux; answer must keep it");
    assert!(
        sdp.contains("H264/90000"),
        "the negotiated codec must appear in an a=rtpmap"
    );
    assert!(has("a=candidate:"), "non-trickle answer needs its candidates inline");
    // Chrome's parser hard-rejects a bare `a=ssrc:<id>` (RFC 5576 wants `<id> <attr>:<val>`),
    // which libdatachannel emits when the track has no cname — the original Bug A.
    let ssrc_lines: Vec<&str> = sdp.lines().filter(|l| l.starts_with("a=ssrc:")).collect();
    assert!(!ssrc_lines.is_empty(), "sendonly answer should declare its ssrc");
    for l in &ssrc_lines {
        assert!(
            l.split_whitespace().count() >= 2,
            "bare a=ssrc line — Chrome rejects this: {l}"
        );
    }
    assert!(
        ssrc_lines.iter().any(|l| l.contains("cname:")),
        "ssrc must carry a cname: {ssrc_lines:?}"
    );
    // We send FU-A, which packetization-mode=0 forbids: the negotiated PT must be one the
    // offer marked packetization-mode=1 (102 here; 104 is the mode=0 twin).
    assert!(
        sdp.contains("a=rtpmap:102 H264/90000") && !sdp.contains("a=rtpmap:104"),
        "answer must pick the packetization-mode=1 payload type"
    );
    // BUNDLE: the offer had a=group:BUNDLE 0. Chrome (balanced policy) tolerates a missing
    // group in the answer, but max-bundle rejects it — flag it loudly either way.
    if !has("a=group:BUNDLE") {
        println!("WARNING: answer lacks a=group:BUNDLE (offer had it)");
    }
}
