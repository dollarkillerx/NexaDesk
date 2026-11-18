//! An AI agent's looks at the host (`nexa_session::agent`, docs/MCP.md §13): one
//! request on a stream of its own, then the reply and the screenshot in pieces.

use nexa_session::{
    agent::{AgentBody, AgentMsg, MAX_PNG, ObserveReply, ObserveRequest},
    framed::open_bi,
    handshake::FLUSH_TIMEOUT,
};
use nexa_wire::framing::StreamType;
use quinn::Connection;

/// The reply, and the PNG if one was asked for (empty otherwise).
pub type Look = Result<(ObserveReply, Vec<u8>), String>;

pub(crate) async fn observe(conn: Connection, request: ObserveRequest) -> Look {
    let (mut send, mut recv) = open_bi(&conn, StreamType::Agent)
        .await
        .map_err(|e| format!("could not ask the host: {e}"))?;
    send.send(&AgentMsg::from(AgentBody::Observe(request)))
        .await
        .map_err(|e| format!("could not ask the host: {e}"))?;
    send.finish_and_flush(FLUSH_TIMEOUT).await;
    let reply = match recv.next::<AgentMsg>().await {
        Ok(Some(AgentMsg {
            body: Some(AgentBody::Reply(r)),
        })) => r,
        Ok(Some(AgentMsg {
            body: Some(AgentBody::Refused(why)),
        })) => return Err(why),
        Ok(_) => return Err("the host did not answer".into()),
        Err(e) => return Err(format!("the host did not answer: {e}")),
    };
    let want = reply.png_len as usize;
    if want > MAX_PNG {
        return Err("the host's screenshot is too large".into());
    }
    let mut png = Vec::with_capacity(want);
    while png.len() < want {
        match recv.next::<AgentMsg>().await {
            Ok(Some(AgentMsg {
                body: Some(AgentBody::Png(piece)),
            })) => png.extend_from_slice(&piece),
            _ => return Err("the screenshot was cut short".into()),
        }
    }
    if png.len() != want {
        return Err("the screenshot did not add up".into());
    }
    recv.stop();
    Ok((reply, png))
}
