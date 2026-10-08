//! Splitting a remote file into independently downloadable byte ranges.

use crate::config::SegmentPolicy;

/// One byte range of the remote file, plus how much of it is already on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Byte offset of the first byte of this range in the remote file.
    pub start: u64,
    /// Length of the range.
    pub len: u64,
    /// Bytes already written for this range.
    pub done: u64,
}

impl Segment {
    pub fn new(start: u64, len: u64) -> Self {
        Self {
            start,
            len,
            done: 0,
        }
    }

    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    pub fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.done)
    }

    pub fn is_complete(&self) -> bool {
        self.done >= self.len
    }

    /// Absolute offset of the next byte to fetch.
    pub fn resume_offset(&self) -> u64 {
        self.start + self.done
    }

    /// Mark `n` more bytes as written, clamped to the range.
    pub fn advance(&mut self, n: u64) {
        self.done = (self.done + n).min(self.len);
    }

    pub fn is_valid(&self) -> bool {
        self.len > 0 && self.done <= self.len
    }
}

/// Decide how to split `size` bytes across at most `policy.connections` streams.
///
/// Segments are deliberately smaller than the per-connection share so that a
/// slow connection cannot hold up the end of the transfer: workers pull from a
/// shared queue, so the work balances itself out.
pub fn plan_segments(size: u64, policy: &SegmentPolicy) -> Vec<Segment> {
    if size == 0 {
        return Vec::new();
    }

    let connections = policy.connections.max(1) as u64;
    let min_segment = policy.min_segment.clamp(1, size);

    let segment_size = match policy.segment_size {
        Some(explicit) => explicit.clamp(1, size),
        None => {
            // Aim for a few segments per connection, then round up to the
            // minimum so that tiny files stay a single request.
            let target_count =
                connections.saturating_mul(policy.segments_per_connection.max(1) as u64);
            let ideal = size.div_ceil(target_count.max(1));
            ideal.max(min_segment)
        }
    };

    // Bound the segment count so the resume state file stays small.
    let count = size.div_ceil(segment_size);
    let segment_size = if count > policy.max_segments.max(1) as u64 {
        size.div_ceil(policy.max_segments.max(1) as u64)
    } else {
        segment_size
    };

    let mut segments = Vec::new();
    let mut start = 0u64;
    while start < size {
        let len = segment_size.min(size - start);
        segments.push(Segment::new(start, len));
        start += len;
    }
    segments
}

/// Re-validate a stored segment list against the size the server reports now.
///
/// Returns `None` when the list cannot describe the current file, which means
/// the remote file changed and the partial download has to be restarted.
pub fn validate_resumed(segments: &[Segment], size: u64) -> Option<Vec<Segment>> {
    if segments.is_empty() {
        return if size == 0 { Some(Vec::new()) } else { None };
    }

    let mut expected_start = 0u64;
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        if !segment.is_valid() || segment.start != expected_start || segment.end() > size {
            return None;
        }
        expected_start = segment.end();
        out.push(segment.clone());
    }

    if expected_start != size {
        return None;
    }
    Some(out)
}

/// Total bytes already on disk across all segments.
pub fn completed_bytes(segments: &[Segment]) -> u64 {
    segments.iter().map(|s| s.done).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(connections: usize) -> SegmentPolicy {
        SegmentPolicy {
            connections,
            ..SegmentPolicy::default()
        }
    }

    #[test]
    fn empty_file_has_no_segments() {
        assert!(plan_segments(0, &policy(4)).is_empty());
    }

    #[test]
    fn segments_cover_the_file_exactly() {
        for size in [1u64, 1023, 1024, 10 * 1024 * 1024, 100 * 1024 * 1024 + 7] {
            let segments = plan_segments(size, &policy(4));
            assert_eq!(segments.first().unwrap().start, 0);
            assert_eq!(segments.last().unwrap().end(), size);
            let total: u64 = segments.iter().map(|s| s.len).sum();
            assert_eq!(total, size, "segments must tile the file for size {size}");
            for pair in segments.windows(2) {
                assert_eq!(pair[0].end(), pair[1].start, "segments must be contiguous");
            }
        }
    }

    #[test]
    fn explicit_segment_size_is_honoured() {
        let mut p = policy(4);
        p.segment_size = Some(1024 * 1024);
        let segments = plan_segments(3 * 1024 * 1024 + 10, &p);
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0].len, 1024 * 1024);
        assert_eq!(segments[3].len, 10);
    }

    #[test]
    fn small_files_stay_in_one_piece() {
        let segments = plan_segments(4096, &policy(8));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].len, 4096);
    }

    #[test]
    fn segment_count_is_capped() {
        let mut p = policy(4);
        p.max_segments = 16;
        p.segment_size = Some(1);
        let segments = plan_segments(1024, &p);
        assert_eq!(segments.len(), 16);
        let total: u64 = segments.iter().map(|s| s.len).sum();
        assert_eq!(total, 1024);
    }

    #[test]
    fn advancing_clamps_to_the_segment() {
        let mut segment = Segment::new(100, 10);
        segment.advance(4);
        assert_eq!(segment.done, 4);
        assert_eq!(segment.resume_offset(), 104);
        segment.advance(100);
        assert_eq!(segment.done, 10);
        assert!(segment.is_complete());
        assert_eq!(segment.remaining(), 0);
    }

    #[test]
    fn validates_a_resumable_plan() {
        let planned = plan_segments(4096, &policy(2));
        let validated = validate_resumed(&planned, 4096).expect("plan should survive");
        assert_eq!(validated, planned);
    }

    #[test]
    fn rejects_a_plan_for_a_changed_file() {
        let planned = plan_segments(4096, &policy(2));
        assert!(validate_resumed(&planned, 8192).is_none());
        assert!(validate_resumed(&planned, 4095).is_none());

        let mut broken = planned.clone();
        broken[0].start = 1;
        assert!(validate_resumed(&broken, 4096).is_none());

        let mut over = planned.clone();
        over.last_mut().unwrap().done = over.last().unwrap().len + 1;
        assert!(validate_resumed(&over, 4096).is_none());
    }

    #[test]
    fn counts_completed_bytes() {
        let mut segments = plan_segments(100, &policy(2));
        let first_len = segments[0].len;
        segments[0].advance(first_len);
        assert_eq!(completed_bytes(&segments), first_len);
    }
}
