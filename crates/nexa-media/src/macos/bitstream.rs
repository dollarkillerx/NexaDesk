//! Annex-B and length-prefixed NAL handling, and parameter-set extraction.
//!
//! Decoder input arrives from the network, so nothing here may panic on any
//! input and nothing may index past a length field it read from the stream.

/// H.264 and HEVC differ only in how a NAL unit type is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalKind {
    Vps,
    Sps,
    Pps,
    AccessUnitDelimiter,
    Slice { idr: bool },
    Other,
}

/// Classifies a NAL unit from its header. A unit too short to have a complete
/// header is `Other`.
pub fn nal_kind(codec: NalCodec, nal: &[u8]) -> NalKind {
    match codec {
        NalCodec::H264 => match nal.first().map(|b| b & 0x1f) {
            Some(1) => NalKind::Slice { idr: false },
            Some(5) => NalKind::Slice { idr: true },
            Some(7) => NalKind::Sps,
            Some(8) => NalKind::Pps,
            Some(9) => NalKind::AccessUnitDelimiter,
            _ => NalKind::Other,
        },
        NalCodec::Hevc => {
            if nal.len() < 2 {
                return NalKind::Other;
            }
            match (nal[0] >> 1) & 0x3f {
                // BLA, IDR and CRA are the intra random access point slice types.
                t @ 16..=21 => NalKind::Slice { idr: matches!(t, 19 | 20) },
                0..=9 => NalKind::Slice { idr: false },
                32 => NalKind::Vps,
                33 => NalKind::Sps,
                34 => NalKind::Pps,
                35 => NalKind::AccessUnitDelimiter,
                _ => NalKind::Other,
            }
        }
    }
}

/// Iterates the NAL units of an Annex-B byte stream.
///
/// Bytes before the first start code are ignored, empty units are skipped, and
/// trailing zero bytes are trimmed. A NAL unit cannot end in 0x00, so those are
/// either `trailing_zero_8bits` or the leading zero of a 4-byte start code.
pub fn annexb_nals(data: &[u8]) -> AnnexBNals<'_> {
    AnnexBNals { data, pos: find_start_code(data, 0).map_or(data.len(), |p| p + 3) }
}

pub struct AnnexBNals<'a> {
    data: &'a [u8],
    /// Offset of the first byte after a start code, or `data.len()` when done.
    pos: usize,
}

impl<'a> Iterator for AnnexBNals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.data.len() {
            let start = self.pos;
            let (end, next) = match find_start_code(self.data, start) {
                Some(p) => (p, p + 3),
                None => (self.data.len(), self.data.len()),
            };
            self.pos = next;
            let nal = trim_trailing_zeros(self.data.get(start..end)?);
            if !nal.is_empty() {
                return Some(nal);
            }
        }
        None
    }
}

fn trim_trailing_zeros(mut s: &[u8]) -> &[u8] {
    while let [head @ .., 0] = s {
        s = head;
    }
    s
}

/// Finds the next `00 00 01` at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        match data[i + 2] {
            1 if data[i] == 0 && data[i + 1] == 0 => return Some(i),
            // No start code can begin at i, i+1 or i+2 if this byte is not 0 or 1.
            b if b > 1 => i += 3,
            _ => i += 1,
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BitstreamError {
    #[error("NAL length field size {0} is not 1 to 4")]
    BadLengthSize(usize),
    #[error("truncated length-prefixed sample")]
    Truncated,
    #[error("too many parameter sets in one access unit")]
    TooManyParameterSets,
    #[error("parameter set longer than {MAX_PARAMETER_SET_LEN} bytes")]
    ParameterSetTooLong,
    #[error("incomplete parameter sets")]
    IncompleteParameterSets,
}

/// Converts one length-prefixed sample to Annex-B, appending to `out`. On error
/// `out` is left as it was found.
pub fn length_prefixed_to_annexb(
    sample: &[u8],
    length_size: usize,
    out: &mut Vec<u8>,
) -> Result<(), BitstreamError> {
    let original_len = out.len();
    let mut rest = sample;
    while !rest.is_empty() {
        let (Some(len), Some(after_len)) = (read_length(rest, length_size)?, rest.get(length_size..))
        else {
            out.truncate(original_len);
            return Err(BitstreamError::Truncated);
        };
        let Some(nal) = after_len.get(..len) else {
            out.truncate(original_len);
            return Err(BitstreamError::Truncated);
        };
        append_annexb_nal(nal, out);
        rest = &after_len[len..];
    }
    Ok(())
}

fn read_length(data: &[u8], length_size: usize) -> Result<Option<usize>, BitstreamError> {
    if !(1..=4).contains(&length_size) {
        return Err(BitstreamError::BadLengthSize(length_size));
    }
    let Some(field) = data.get(..length_size) else { return Ok(None) };
    Ok(Some(field.iter().fold(0usize, |v, b| (v << 8) | usize::from(*b))))
}

/// Appends `nal` preceded by a 4-byte start code. An empty unit writes nothing.
pub fn append_annexb_nal(nal: &[u8], out: &mut Vec<u8>) {
    if !nal.is_empty() {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
}

/// Appends `nal` with a 4-byte big-endian length prefix. Fails rather than
/// truncates if the unit does not fit the length field.
pub fn append_length_prefixed_nal(nal: &[u8], out: &mut Vec<u8>) -> Result<(), BitstreamError> {
    let len = u32::try_from(nal.len()).map_err(|_| BitstreamError::Truncated)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(nal);
    Ok(())
}

/// More than any encoder we use emits, far fewer than the bitstream syntax allows.
const MAX_SETS_PER_KIND: usize = 8;
/// Parameter sets are tens of bytes; anything huge in-band is malformed.
pub const MAX_PARAMETER_SET_LEN: usize = 8 * 1024;

/// The parameter sets carried in one access unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub vps: Vec<Vec<u8>>,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

impl ParameterSets {
    pub fn is_empty(&self) -> bool {
        self.vps.is_empty() && self.sps.is_empty() && self.pps.is_empty()
    }

    /// Everything a decoder needs is present for `codec`.
    pub fn is_complete(&self, codec: NalCodec) -> bool {
        !self.sps.is_empty() && !self.pps.is_empty() && (codec == NalCodec::H264 || !self.vps.is_empty())
    }

    /// The sets in the order the CoreMedia format description constructors want.
    pub fn ordered(&self) -> impl Iterator<Item = &[u8]> {
        self.vps.iter().chain(&self.sps).chain(&self.pps).map(Vec::as_slice)
    }
}

/// An Annex-B access unit split into what the decoder needs.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedAccessUnit<'a> {
    pub sets: ParameterSets,
    /// Slice and SEI units, in stream order, without parameter sets or delimiters.
    pub payload: Vec<&'a [u8]>,
    pub has_idr: bool,
}

/// Splits an access unit into parameter sets and payload units.
pub fn parse_access_unit(codec: NalCodec, au: &[u8]) -> Result<ParsedAccessUnit<'_>, BitstreamError> {
    let mut sets = ParameterSets::default();
    let mut payload = Vec::new();
    let mut has_idr = false;
    for nal in annexb_nals(au) {
        match nal_kind(codec, nal) {
            NalKind::Vps => push_parameter_set(&mut sets.vps, nal)?,
            NalKind::Sps => push_parameter_set(&mut sets.sps, nal)?,
            NalKind::Pps => push_parameter_set(&mut sets.pps, nal)?,
            NalKind::AccessUnitDelimiter => {}
            kind => {
                has_idr |= kind == NalKind::Slice { idr: true };
                payload.push(nal);
            }
        }
    }
    Ok(ParsedAccessUnit { sets, payload, has_idr })
}

fn push_parameter_set(sets: &mut Vec<Vec<u8>>, nal: &[u8]) -> Result<(), BitstreamError> {
    if nal.len() > MAX_PARAMETER_SET_LEN {
        return Err(BitstreamError::ParameterSetTooLong);
    }
    // Repeats within one access unit are legal and mean nothing.
    if sets.iter().any(|s| s == nal) {
        return Ok(());
    }
    if sets.len() == MAX_SETS_PER_KIND {
        return Err(BitstreamError::TooManyParameterSets);
    }
    sets.push(nal.to_vec());
    Ok(())
}

/// Encodes `payload` units as a single 4-byte-length-prefixed sample into `out`.
pub fn payload_to_length_prefixed(payload: &[&[u8]], out: &mut Vec<u8>) -> Result<(), BitstreamError> {
    out.clear();
    for nal in payload {
        append_length_prefixed_nal(nal, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(data: &[u8]) -> Vec<Vec<u8>> {
        annexb_nals(data).map(<[u8]>::to_vec).collect()
    }

    /// A deterministic byte stream with the zeros and ones the parser cares about.
    fn prng(seed: u32, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2).wrapping_add(1);
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                match (x >> 16) & 7 {
                    0..=2 => 0,
                    3 => 1,
                    _ => (x >> 24) as u8,
                }
            })
            .collect()
    }

    #[test]
    fn splits_mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        assert_eq!(collect(&data), vec![vec![0x67, 1, 2], vec![0x68, 3], vec![0x65, 4]]);
    }

    #[test]
    fn ignores_garbage_before_first_start_code() {
        assert_eq!(collect(&[9, 9, 9, 0, 0, 1, 0x65, 7]), vec![vec![0x65, 7]]);
    }

    #[test]
    fn empty_and_startcode_only_input_yields_nothing() {
        assert!(collect(&[]).is_empty());
        assert!(collect(&[0]).is_empty());
        assert!(collect(&[0, 0, 1]).is_empty());
        assert!(collect(&[0, 0, 0, 1, 0, 0, 0, 1]).is_empty());
        assert!(collect(&[1, 2, 3, 4, 5]).is_empty());
    }

    #[test]
    fn trims_trailing_zeros_and_skips_empty_units() {
        assert_eq!(collect(&[0, 0, 1, 0x65, 8, 0, 0, 0]), vec![vec![0x65, 8]]);
        assert_eq!(collect(&[0, 0, 1, 0, 0, 1, 0x65, 9]), vec![vec![0x65, 9]]);
    }

    #[test]
    fn unit_ending_at_buffer_end_is_kept() {
        assert_eq!(collect(&[0, 0, 1, 0x41]), vec![vec![0x41]]);
    }

    #[test]
    fn classifies_h264() {
        assert_eq!(nal_kind(NalCodec::H264, &[0x67]), NalKind::Sps);
        assert_eq!(nal_kind(NalCodec::H264, &[0x68]), NalKind::Pps);
        assert_eq!(nal_kind(NalCodec::H264, &[0x65]), NalKind::Slice { idr: true });
        assert_eq!(nal_kind(NalCodec::H264, &[0x41]), NalKind::Slice { idr: false });
        assert_eq!(nal_kind(NalCodec::H264, &[0x09]), NalKind::AccessUnitDelimiter);
        assert_eq!(nal_kind(NalCodec::H264, &[]), NalKind::Other);
    }

    #[test]
    fn classifies_hevc() {
        let h = |t: u8| [t << 1, 1];
        assert_eq!(nal_kind(NalCodec::Hevc, &h(32)), NalKind::Vps);
        assert_eq!(nal_kind(NalCodec::Hevc, &h(33)), NalKind::Sps);
        assert_eq!(nal_kind(NalCodec::Hevc, &h(34)), NalKind::Pps);
        assert_eq!(nal_kind(NalCodec::Hevc, &h(35)), NalKind::AccessUnitDelimiter);
        assert_eq!(nal_kind(NalCodec::Hevc, &h(19)), NalKind::Slice { idr: true });
        assert_eq!(nal_kind(NalCodec::Hevc, &h(20)), NalKind::Slice { idr: true });
        assert_eq!(nal_kind(NalCodec::Hevc, &h(21)), NalKind::Slice { idr: false });
        assert_eq!(nal_kind(NalCodec::Hevc, &h(1)), NalKind::Slice { idr: false });
        assert_eq!(nal_kind(NalCodec::Hevc, &[0x40]), NalKind::Other);
    }

    #[test]
    fn length_prefixed_round_trip() {
        let nals: [&[u8]; 3] = [&[0x67, 1, 2, 3], &[0x68, 4], &[0x65, 5, 6]];
        let mut sample = Vec::new();
        payload_to_length_prefixed(&nals, &mut sample).unwrap();
        let mut annexb = vec![0xee];
        length_prefixed_to_annexb(&sample, 4, &mut annexb).unwrap();
        assert_eq!(annexb[0], 0xee);
        assert_eq!(collect(&annexb[1..]), nals.iter().map(|n| n.to_vec()).collect::<Vec<_>>());
    }

    #[test]
    fn length_prefixed_honours_other_field_sizes() {
        let mut out = Vec::new();
        length_prefixed_to_annexb(&[0, 2, 0x41, 9], 2, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41, 9]);
        out.clear();
        length_prefixed_to_annexb(&[1, 0x41], 1, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn length_prefixed_rejects_bad_input_and_keeps_output() {
        let mut out = vec![1, 2, 3];
        for (sample, size) in [
            (&[0, 0, 0, 9, 0x41][..], 4),    // length runs past the end
            (&[0, 0, 0][..], 4),             // length field itself truncated
            (&[0xff, 0xff, 0xff, 0xff, 1][..], 4),
            (&[0, 0, 0, 1, 0x41, 0, 0][..], 4), // second field truncated
        ] {
            assert_eq!(
                length_prefixed_to_annexb(sample, size, &mut out),
                Err(BitstreamError::Truncated),
                "{sample:?}"
            );
            assert_eq!(out, vec![1, 2, 3]);
        }
        assert_eq!(length_prefixed_to_annexb(&[1, 2], 0, &mut out), Err(BitstreamError::BadLengthSize(0)));
        assert_eq!(length_prefixed_to_annexb(&[1, 2], 5, &mut out), Err(BitstreamError::BadLengthSize(5)));
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn zero_length_units_are_skipped() {
        let mut out = Vec::new();
        length_prefixed_to_annexb(&[0, 0, 0, 0, 0, 0, 0, 1, 0x41], 4, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn parses_h264_key_frame() {
        let au = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x09, 0x10, 0, 0, 1, 0x65, 3, 0, 0, 1, 0x06, 4];
        let p = parse_access_unit(NalCodec::H264, &au).unwrap();
        assert_eq!(p.sets.sps, vec![vec![0x67, 1]]);
        assert_eq!(p.sets.pps, vec![vec![0x68, 2]]);
        assert!(p.sets.vps.is_empty());
        assert!(p.sets.is_complete(NalCodec::H264));
        assert!(p.has_idr);
        assert_eq!(p.payload, vec![&[0x65u8, 3][..], &[0x06, 4][..]]);
    }

    #[test]
    fn parses_hevc_key_frame_in_order() {
        let n = |t: u8, b: u8| vec![0, 0, 0, 1, t << 1, 1, b];
        let au: Vec<u8> = [n(32, 1), n(33, 2), n(34, 3), n(35, 4), n(19, 5)].concat();
        let p = parse_access_unit(NalCodec::Hevc, &au).unwrap();
        assert!(p.sets.is_complete(NalCodec::Hevc));
        assert!(p.has_idr);
        assert_eq!(p.payload.len(), 1);
        let ordered: Vec<_> = p.sets.ordered().map(<[u8]>::to_vec).collect();
        assert_eq!(ordered, vec![vec![32 << 1, 1, 1], vec![33 << 1, 1, 2], vec![34 << 1, 1, 3]]);
    }

    #[test]
    fn parameter_sets_incomplete_or_absent() {
        let sps_only = [0, 0, 1, 0x67, 1];
        let p = parse_access_unit(NalCodec::H264, &sps_only).unwrap();
        assert!(!p.sets.is_complete(NalCodec::H264));
        assert!(!p.sets.is_empty());

        let hevc_no_vps = [0, 0, 1, 33 << 1, 1, 7, 0, 0, 1, 34 << 1, 1, 8];
        assert!(!parse_access_unit(NalCodec::Hevc, &hevc_no_vps).unwrap().sets.is_complete(NalCodec::Hevc));

        let p_slice = [0, 0, 1, 0x41, 1];
        let p = parse_access_unit(NalCodec::H264, &p_slice).unwrap();
        assert!(p.sets.is_empty());
        assert!(!p.has_idr);
        assert_eq!(p.payload.len(), 1);
    }

    #[test]
    fn repeated_parameter_sets_are_collapsed() {
        let au = [0, 0, 1, 0x67, 1, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2];
        assert_eq!(parse_access_unit(NalCodec::H264, &au).unwrap().sets.sps.len(), 1);
    }

    #[test]
    fn parameter_set_limits() {
        let mut too_many = Vec::new();
        for i in 0..=MAX_SETS_PER_KIND as u8 {
            too_many.extend_from_slice(&[0, 0, 1, 0x67, i + 1]);
        }
        assert_eq!(
            parse_access_unit(NalCodec::H264, &too_many),
            Err(BitstreamError::TooManyParameterSets)
        );

        let mut long = vec![0, 0, 1, 0x67];
        long.resize(4 + MAX_PARAMETER_SET_LEN + 1, 0x55);
        assert_eq!(
            parse_access_unit(NalCodec::H264, &long),
            Err(BitstreamError::ParameterSetTooLong)
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        for seed in 0..400u32 {
            let data = prng(seed, (seed as usize * 7) % 900);
            for codec in [NalCodec::H264, NalCodec::Hevc] {
                let _ = parse_access_unit(codec, &data);
                for nal in annexb_nals(&data) {
                    let _ = nal_kind(codec, nal);
                }
            }
            for size in 0..6 {
                let mut out = Vec::new();
                let _ = length_prefixed_to_annexb(&data, size, &mut out);
            }
        }
    }

    #[test]
    fn annexb_iterator_only_returns_nonempty_subslices() {
        for seed in 0..200u32 {
            let data = prng(seed, 256);
            let total: usize = annexb_nals(&data).inspect(|n| assert!(!n.is_empty())).map(<[u8]>::len).sum();
            assert!(total <= data.len());
        }
    }
}
