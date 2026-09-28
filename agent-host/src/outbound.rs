// 跨连接的出站队列。
//
// 一次调用的任务不绑在某一条 WS 上：在线时帧立刻写进当前连接；
// 离线时 invocation_finish / approval_request 留在这里，下次 host_ready
// 之后按入队顺序补发。invocation_event 离线直接丢掉（finish 里有全文和步骤）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

const PRE_CANCEL_MAX: usize = 256;
const PRE_CANCEL_TTL: Duration = Duration::from_secs(10 * 60);

struct Held {
    seq: u64,
    inv: Uuid,
    body: String,
}

struct LaneInner {
    live: Option<mpsc::UnboundedSender<OutItem>>,
    /// 当前连接的写任务。retire 时 abort，否则调用方被取消后它还握着套接字。
    writer: Option<JoinHandle<()>>,
    held: VecDeque<Held>,
    next_seq: u64,
    /// retire 加一。旧连接的写任务带着旧代号，不能再确认补发队列。
    gen: u64,
}

/// 写进当前连接的一帧。Durable 在套接字写成功前一直留在补发队列里。
pub(crate) enum OutItem {
    Eph(Message),
    Durable { seq: u64, body: String },
}

#[derive(Clone)]
pub(crate) struct Outbound {
    inner: Arc<Mutex<LaneInner>>,
}

impl Outbound {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LaneInner {
                live: None,
                writer: None,
                held: VecDeque::new(),
                next_seq: 1,
                gen: 0,
            })),
        }
    }

    /// 当前连接开始写。把还没送达的持久帧按原顺序排进这条通道。
    pub(crate) fn engage(&self, tx: mpsc::UnboundedSender<OutItem>) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.live = Some(tx.clone());
        let gen = g.gen;
        for h in &g.held {
            let _ = tx.send(OutItem::Durable {
                seq: h.seq,
                body: h.body.clone(),
            });
        }
        gen
    }

    /// 记下这条连接的写任务。上一条还在就先停掉。
    pub(crate) fn attach_writer(&self, writer: JoinHandle<()>) {
        let mut g = self.inner.lock().unwrap();
        if let Some(old) = g.writer.replace(writer) {
            old.abort();
        }
    }

    /// 连接没了。之后的持久帧只进队列，不进已经死掉的通道。写任务一并停下，套接字才会关。
    pub(crate) fn retire(&self) {
        let writer = {
            let mut g = self.inner.lock().unwrap();
            g.live = None;
            g.gen = g.gen.wrapping_add(1);
            g.writer.take()
        };
        if let Some(writer) = writer {
            writer.abort();
        }
    }

    /// 套接字写成功。代号对不上说明这是上一条连接的写任务，帧还要补发。
    pub(crate) fn ack(&self, gen: u64, seq: u64) {
        let mut g = self.inner.lock().unwrap();
        if g.gen != gen {
            return;
        }
        if g.held.front().is_some_and(|h| h.seq == seq) {
            g.held.pop_front();
        } else {
            g.held.retain(|h| h.seq != seq);
        }
    }

    pub(crate) fn invocation_ids(&self) -> Vec<Uuid> {
        let g = self.inner.lock().unwrap();
        let mut ids: Vec<Uuid> = g.held.iter().map(|h| h.inv).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// 流式增量。离线或通道已关就丢掉。
    pub(crate) fn emit_ephemeral(&self, msg: Message) {
        let mut g = self.inner.lock().unwrap();
        if let Some(tx) = &g.live {
            if tx.send(OutItem::Eph(msg)).is_err() {
                g.live = None;
            }
        }
    }

    /// finish / approval_request。同一调用按调用顺序留在队列里。
    pub(crate) fn emit_durable(&self, inv: Uuid, body: String) {
        let mut g = self.inner.lock().unwrap();
        let seq = g.next_seq;
        g.next_seq = g.next_seq.wrapping_add(1);
        g.held.push_back(Held {
            seq,
            inv,
            body: body.clone(),
        });
        if let Some(tx) = &g.live {
            if tx.send(OutItem::Durable { seq, body }).is_err() {
                g.live = None;
            }
        }
    }
}

/// 先到的 invocation_cancel。调用还没登记时记下，之后的 start 直接丢掉。
/// 上限：最近 256 个，或 10 分钟。
#[derive(Clone)]
pub(crate) struct PreCancel {
    items: Arc<Mutex<VecDeque<(Uuid, Instant)>>>,
}

impl PreCancel {
    pub(crate) fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub(crate) fn remember(&self, id: Uuid) {
        let mut q = self.items.lock().unwrap();
        let now = Instant::now();
        purge(&mut q, now);
        q.retain(|(i, _)| *i != id);
        q.push_back((id, now));
        while q.len() > PRE_CANCEL_MAX {
            q.pop_front();
        }
    }

    /// 有过这条提前取消就摘掉并返回 true。
    pub(crate) fn take(&self, id: Uuid) -> bool {
        let mut q = self.items.lock().unwrap();
        let now = Instant::now();
        purge(&mut q, now);
        let before = q.len();
        q.retain(|(i, _)| *i != id);
        q.len() != before
    }
}

fn purge(q: &mut VecDeque<(Uuid, Instant)>, now: Instant) {
    q.retain(|(_, t)| now.saturating_duration_since(*t) < PRE_CANCEL_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(items: &[OutItem]) -> Vec<String> {
        items
            .iter()
            .map(|it| match it {
                OutItem::Eph(Message::Text(t)) => t.to_string(),
                OutItem::Durable { body, .. } => body.clone(),
                OutItem::Eph(_) => "other".into(),
            })
            .collect()
    }

    #[tokio::test]
    async fn offline_keeps_durable_order_and_drops_events() {
        let lane = Outbound::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        lane.emit_ephemeral(Message::Text("event".into()));
        lane.emit_durable(a, "finish-a".into());
        lane.emit_durable(a, "approval-a".into());
        lane.emit_durable(b, "finish-b".into());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let gen = lane.engage(tx);
        let mut got = Vec::new();
        while let Ok(item) = rx.try_recv() {
            got.push(item);
        }
        assert_eq!(bodies(&got), vec!["finish-a", "approval-a", "finish-b"]);
        for item in &got {
            if let OutItem::Durable { seq, .. } = item {
                lane.ack(gen, *seq);
            }
        }
        assert!(lane.invocation_ids().is_empty());

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        lane.engage(tx2);
        assert!(rx2.try_recv().is_err(), "确认过的帧不应再补发");
    }

    #[test]
    fn pre_cancel_caps_at_256_newest() {
        let p = PreCancel::new();
        let mut ids = Vec::new();
        for _ in 0..300 {
            let id = Uuid::new_v4();
            p.remember(id);
            ids.push(id);
        }
        assert!(!p.take(ids[0]), "超过 256 的最旧一条应被丢掉");
        assert!(p.take(ids[299]));
        assert!(!p.take(ids[299]), "take 之后不应还在");
    }
}
