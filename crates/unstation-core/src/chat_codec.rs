//! SDP-over-statement codec — reuses the Polkadot app's chat transport protocol
//! (TECH_SPEC §7.4) so the statement store can broker our mesh's WebRTC offers.
//!
//! This mirrors the app's `ChatMessageStatementContent` DataChannel variants
//! wire-for-wire (SCALE, with the same enum indices) and adds a **`StreamMesh`**
//! purpose to `DataChannelPurpose`. The variant indices and field order MUST match
//! the other codecs for interop — a coordinated change:
//!   - TS   `triangle-js-sdks/host-chat/src/codec/message.ts`
//!   - Android `polkadot-android-community/feature/chats/transport-protocol/…/ChatMessageStatementContent.kt`
//!   - iOS  `polkadot-app-ios-v2/…/Chat/Model/RemoteChatMessage.swift`
//!
//! The index-layout tests below are the guard that keeps this side compatible.

use crate::signaling::SignalMsg;
use parity_scale_codec::{Decode, Encode};

/// Purpose of a data-channel session. `AudioCall`/`VideoCall` are the app's; we add
/// `StreamMesh` for the streaming mesh.
#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataChannelPurpose {
    #[codec(index = 0)]
    AudioCall,
    #[codec(index = 1)]
    VideoCall,
    #[codec(index = 2)]
    StreamMesh,
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub struct DataChannelOffer {
    pub sdp: Vec<u8>,
    pub purpose: DataChannelPurpose,
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub struct DataChannelAnswer {
    pub offer_message_id: String,
    pub sdp: Vec<u8>,
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub struct DataChannelIceCandidate {
    pub offer_message_id: String,
    pub sdp: Vec<u8>,
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub struct DataChannelClosed {
    pub offer_message_id: String,
}

/// The subset of `ChatMessageStatementContent` we emit/parse. Explicit `index`
/// values pin the wire layout to the app's enum (offer=8 … closed=11).
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub enum ChatMessageContent {
    #[codec(index = 8)]
    DataChannelOffer(DataChannelOffer),
    #[codec(index = 9)]
    DataChannelAnswer(DataChannelAnswer),
    #[codec(index = 10)]
    DataChannelIceCandidate(DataChannelIceCandidate),
    #[codec(index = 11)]
    DataChannelClosed(DataChannelClosed),
}

impl ChatMessageContent {
    /// Wrap an engine [`SignalMsg`] as chat content with the `StreamMesh` purpose.
    pub fn from_signal(msg: &SignalMsg) -> Self {
        match msg {
            SignalMsg::Offer { sdp } => ChatMessageContent::DataChannelOffer(DataChannelOffer {
                sdp: sdp.clone(),
                purpose: DataChannelPurpose::StreamMesh,
            }),
            SignalMsg::Answer { offer_id, sdp } => {
                ChatMessageContent::DataChannelAnswer(DataChannelAnswer {
                    offer_message_id: offer_id.clone(),
                    sdp: sdp.clone(),
                })
            }
            SignalMsg::IceCandidate { offer_id, sdp } => {
                ChatMessageContent::DataChannelIceCandidate(DataChannelIceCandidate {
                    offer_message_id: offer_id.clone(),
                    sdp: sdp.clone(),
                })
            }
            SignalMsg::Closed { offer_id } => {
                ChatMessageContent::DataChannelClosed(DataChannelClosed {
                    offer_message_id: offer_id.clone(),
                })
            }
        }
    }

    /// Recover an engine [`SignalMsg`].
    pub fn to_signal(&self) -> SignalMsg {
        match self {
            ChatMessageContent::DataChannelOffer(o) => SignalMsg::Offer { sdp: o.sdp.clone() },
            ChatMessageContent::DataChannelAnswer(a) => SignalMsg::Answer {
                offer_id: a.offer_message_id.clone(),
                sdp: a.sdp.clone(),
            },
            ChatMessageContent::DataChannelIceCandidate(c) => SignalMsg::IceCandidate {
                offer_id: c.offer_message_id.clone(),
                sdp: c.sdp.clone(),
            },
            ChatMessageContent::DataChannelClosed(c) => SignalMsg::Closed {
                offer_id: c.offer_message_id.clone(),
            },
        }
    }
}

/// Encode a signal as the statement `data` payload.
pub fn encode_signal(msg: &SignalMsg) -> Vec<u8> {
    ChatMessageContent::from_signal(msg).encode()
}

/// Decode a statement `data` payload back to a signal (None if it isn't a
/// DataChannel content variant).
pub fn decode_signal(bytes: &[u8]) -> Option<SignalMsg> {
    ChatMessageContent::decode(&mut &bytes[..]).ok().map(|c| c.to_signal())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_indices_match_app_wire_layout() {
        // First byte of the SCALE encoding is the enum index.
        let offer = ChatMessageContent::DataChannelOffer(DataChannelOffer {
            sdp: vec![],
            purpose: DataChannelPurpose::StreamMesh,
        })
        .encode();
        assert_eq!(offer[0], 8, "DataChannelOffer must be index 8");
        // ...followed by the offer struct: empty sdp (compact len 0) then purpose byte.
        assert_eq!(*offer.last().unwrap(), 2, "StreamMesh purpose must be index 2");

        let answer = ChatMessageContent::DataChannelAnswer(DataChannelAnswer {
            offer_message_id: String::new(),
            sdp: vec![],
        })
        .encode();
        assert_eq!(answer[0], 9);

        let ice = ChatMessageContent::DataChannelIceCandidate(DataChannelIceCandidate {
            offer_message_id: String::new(),
            sdp: vec![],
        })
        .encode();
        assert_eq!(ice[0], 10);

        let closed = ChatMessageContent::DataChannelClosed(DataChannelClosed {
            offer_message_id: String::new(),
        })
        .encode();
        assert_eq!(closed[0], 11);
    }

    #[test]
    fn purpose_indices() {
        assert_eq!(DataChannelPurpose::AudioCall.encode(), vec![0]);
        assert_eq!(DataChannelPurpose::VideoCall.encode(), vec![1]);
        assert_eq!(DataChannelPurpose::StreamMesh.encode(), vec![2]);
    }

    #[test]
    fn signal_roundtrip_through_chat_content() {
        let cases = [
            SignalMsg::Offer { sdp: b"v=0...offer".to_vec() },
            SignalMsg::Answer { offer_id: "off-1".into(), sdp: b"v=0...answer".to_vec() },
            SignalMsg::IceCandidate { offer_id: "off-1".into(), sdp: b"candidate:...".to_vec() },
            SignalMsg::Closed { offer_id: "off-1".into() },
        ];
        for msg in cases {
            let bytes = encode_signal(&msg);
            assert_eq!(decode_signal(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn offer_carries_stream_mesh_purpose() {
        let bytes = encode_signal(&SignalMsg::Offer { sdp: b"x".to_vec() });
        match ChatMessageContent::decode(&mut &bytes[..]).unwrap() {
            ChatMessageContent::DataChannelOffer(o) => {
                assert_eq!(o.purpose, DataChannelPurpose::StreamMesh)
            }
            other => panic!("expected offer, got {other:?}"),
        }
    }
}
