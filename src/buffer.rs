//! Buffer circular de segmentos de vídeo: mantém apenas os últimos N segundos
//! e permite selecionar os segmentos que compõem um clip retroativo.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub path: PathBuf,
    pub duration: Duration,
}

pub struct SegmentBuffer {
    window: Duration,
    segments: VecDeque<Segment>,
}

impl SegmentBuffer {
    pub fn new(window: Duration) -> Self {
        Self { window, segments: VecDeque::new() }
    }

    /// Adiciona um segmento; devolve os segmentos expirados (a apagar do disco).
    pub fn push(&mut self, segment: Segment) -> Vec<Segment> {
        self.segments.push_back(segment);
        let mut expired = Vec::new();
        while self.segments.len() > 1 && self.total() > self.window {
            expired.extend(self.segments.pop_front());
        }
        expired
    }

    pub fn total(&self) -> Duration {
        self.segments.iter().map(|s| s.duration).sum()
    }

    /// Segmentos suficientes para cobrir os últimos `span`, em ordem cronológica.
    pub fn last(&self, span: Duration) -> Vec<Segment> {
        let mut covered = Duration::ZERO;
        let mut picked: Vec<Segment> = self
            .segments
            .iter()
            .rev()
            .take_while(|s| {
                let more = covered < span;
                covered += s.duration;
                more
            })
            .cloned()
            .collect();
        picked.reverse();
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(n: u32, secs: u64) -> Segment {
        Segment { path: format!("seg{n}.mp4").into(), duration: Duration::from_secs(secs) }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn empty_buffer_has_zero_total() {
        let b = SegmentBuffer::new(secs(60));
        assert_eq!(b.total(), Duration::ZERO);
        assert!(b.last(secs(10)).is_empty());
    }

    #[test]
    fn keeps_segments_within_window() {
        let mut b = SegmentBuffer::new(secs(15));
        assert!(b.push(seg(1, 5)).is_empty());
        assert!(b.push(seg(2, 5)).is_empty());
        assert!(b.push(seg(3, 5)).is_empty());
        assert_eq!(b.total(), secs(15));
    }

    #[test]
    fn evicts_oldest_when_window_exceeded() {
        let mut b = SegmentBuffer::new(secs(15));
        for n in 1..=3 {
            b.push(seg(n, 5));
        }
        let expired = b.push(seg(4, 5));
        assert_eq!(expired, vec![seg(1, 5)]);
        assert_eq!(b.total(), secs(15));
    }

    #[test]
    fn evicts_several_when_one_large_segment_arrives() {
        let mut b = SegmentBuffer::new(secs(10));
        b.push(seg(1, 5));
        b.push(seg(2, 5));
        let expired = b.push(seg(3, 10));
        assert_eq!(expired, vec![seg(1, 5), seg(2, 5)]);
    }

    #[test]
    fn never_evicts_the_only_segment() {
        let mut b = SegmentBuffer::new(secs(5));
        assert!(b.push(seg(1, 8)).is_empty());
        assert_eq!(b.total(), secs(8));
    }

    #[test]
    fn last_returns_enough_segments_in_order() {
        let mut b = SegmentBuffer::new(secs(60));
        for n in 1..=5 {
            b.push(seg(n, 5));
        }
        // 12s => precisa de 3 segmentos (15s) para cobrir
        assert_eq!(b.last(secs(12)), vec![seg(3, 5), seg(4, 5), seg(5, 5)]);
    }

    #[test]
    fn last_larger_than_buffer_returns_everything() {
        let mut b = SegmentBuffer::new(secs(60));
        b.push(seg(1, 5));
        b.push(seg(2, 5));
        assert_eq!(b.last(secs(600)), vec![seg(1, 5), seg(2, 5)]);
    }
}
