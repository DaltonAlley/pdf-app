use std::io::{self, Read, Write};

use lopdf::{Dictionary, Object, Stream};

use crate::error::{AppError, AppResult};

const MAX_INTERMEDIATE_CONTENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn decode_page_content_stream(stream: &Stream, limit: usize) -> AppResult<Vec<u8>> {
    if stream.dict.get(b"Filter").is_err() {
        if stream.content.len() > limit {
            return Err(AppError::payload_too_large(
                "source page content is too large",
            ));
        }
        return Ok(stream.content.clone());
    }

    let filters = stream.filters().map_err(|err| {
        AppError::bad_request(format!(
            "source page uses unsupported multi-stream content encoding: {err}"
        ))
    })?;
    let mut decoded = stream.content.clone();
    for (index, filter) in filters.iter().enumerate() {
        let params = filter_decode_params(stream, index, filters.len())?;
        ensure_content_predictor_is_plain(params)?;
        let filter_limit = if index + 1 == filters.len() {
            limit
        } else {
            MAX_INTERMEDIATE_CONTENT_BYTES
        };
        decoded = match *filter {
            b"FlateDecode" => decode_flate(&decoded, filter_limit)?,
            b"ASCIIHexDecode" => decode_ascii_hex(&decoded, filter_limit)?,
            b"ASCII85Decode" => decode_ascii85(&decoded, filter_limit)?,
            b"LZWDecode" => decode_lzw(&decoded, params, filter_limit)?,
            b"RunLengthDecode" => decode_run_length(&decoded, filter_limit)?,
            _ => return Err(unsupported_content_encoding()),
        };
    }

    Ok(decoded)
}

fn filter_decode_params(
    stream: &Stream,
    index: usize,
    filter_count: usize,
) -> AppResult<Option<&Dictionary>> {
    let Ok(params) = stream.dict.get(b"DecodeParms") else {
        return Ok(None);
    };
    match params {
        Object::Null => Ok(None),
        Object::Dictionary(params) if filter_count == 1 => Ok(Some(params)),
        Object::Array(params) if params.len() == filter_count => match params.get(index) {
            Some(Object::Null) => Ok(None),
            Some(Object::Dictionary(params)) => Ok(Some(params)),
            Some(_) | None => Err(unsupported_content_encoding()),
        },
        _ => Err(unsupported_content_encoding()),
    }
}

fn ensure_content_predictor_is_plain(params: Option<&Dictionary>) -> AppResult<()> {
    let Some(predictor) = params.and_then(|params| params.get(b"Predictor").ok()) else {
        return Ok(());
    };
    if predictor.as_i64().ok() == Some(1) {
        Ok(())
    } else {
        Err(unsupported_content_encoding())
    }
}

fn decode_flate(input: &[u8], limit: usize) -> AppResult<Vec<u8>> {
    let read_limit = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
    let mut decoder = flate2::read::ZlibDecoder::new(input).take(read_limit);
    let mut decoded = Vec::with_capacity(input.len().min(limit));
    decoder.read_to_end(&mut decoded).map_err(|err| {
        AppError::bad_request(format!("source page content could not be decoded: {err}"))
    })?;
    ensure_decoded_limit(decoded, limit)
}

fn decode_ascii85(input: &[u8], limit: usize) -> AppResult<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len().min(limit));
    let mut buffer = 0_u32;
    let mut count = 0_usize;
    let mut index = 0_usize;

    while let Some(&byte) = input.get(index) {
        index += 1;
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'~' && input.get(index) == Some(&b'>') {
            break;
        }
        if byte == b'z' {
            if count != 0 {
                return Err(unsupported_content_encoding());
            }
            append_limited(&mut output, &[0, 0, 0, 0], limit)?;
            continue;
        }
        if !(b'!'..=b'u').contains(&byte) {
            return Err(unsupported_content_encoding());
        }
        buffer = buffer
            .checked_mul(85)
            .and_then(|value| value.checked_add(u32::from(byte - b'!')))
            .ok_or_else(unsupported_content_encoding)?;
        count += 1;
        if count == 5 {
            append_limited(&mut output, &buffer.to_be_bytes(), limit)?;
            buffer = 0;
            count = 0;
        }
    }

    if count == 1 {
        return Err(unsupported_content_encoding());
    }
    if count > 1 {
        for _ in count..5 {
            buffer = buffer
                .checked_mul(85)
                .and_then(|value| value.checked_add(84))
                .ok_or_else(unsupported_content_encoding)?;
        }
        append_limited(&mut output, &buffer.to_be_bytes()[..count - 1], limit)?;
    }
    Ok(output)
}

fn decode_ascii_hex(input: &[u8], limit: usize) -> AppResult<Vec<u8>> {
    let mut output = Vec::with_capacity((input.len() / 2).min(limit));
    let mut high_nibble = None;
    for &byte in input {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'>' {
            break;
        }
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(unsupported_content_encoding()),
        };
        if let Some(high) = high_nibble.take() {
            append_limited(&mut output, &[high << 4 | nibble], limit)?;
        } else {
            high_nibble = Some(nibble);
        }
    }
    if let Some(high) = high_nibble {
        append_limited(&mut output, &[high << 4], limit)?;
    }
    Ok(output)
}

fn decode_run_length(input: &[u8], limit: usize) -> AppResult<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len().min(limit));
    let mut index = 0_usize;
    while let Some(&length) = input.get(index) {
        index += 1;
        match length {
            128 => break,
            0..=127 => {
                let count = usize::from(length) + 1;
                let end = index
                    .checked_add(count)
                    .filter(|end| *end <= input.len())
                    .ok_or_else(unsupported_content_encoding)?;
                append_limited(&mut output, &input[index..end], limit)?;
                index = end;
            }
            129..=255 => {
                let byte = *input.get(index).ok_or_else(unsupported_content_encoding)?;
                index += 1;
                let count = 257 - usize::from(length);
                if output
                    .len()
                    .checked_add(count)
                    .is_none_or(|length| length > limit)
                {
                    return Err(AppError::payload_too_large(
                        "source page content is too large",
                    ));
                }
                output.resize(output.len() + count, byte);
            }
        }
    }
    Ok(output)
}

fn decode_lzw(input: &[u8], params: Option<&Dictionary>, limit: usize) -> AppResult<Vec<u8>> {
    use weezl::{decode::Decoder, BitOrder};

    let early_change = match params.and_then(|params| params.get(b"EarlyChange").ok()) {
        None => true,
        Some(value) => match value.as_i64().ok() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(unsupported_content_encoding()),
        },
    };
    let mut decoder = if early_change {
        Decoder::with_tiff_size_switch(BitOrder::Msb, 8)
    } else {
        Decoder::new(BitOrder::Msb, 8)
    };
    let mut writer = LimitedWriter::new(limit);
    let mut stream = decoder.into_stream(&mut writer);
    stream.set_buffer_size(8 * 1024);
    let result = stream.decode_all(input);
    if writer.exceeded {
        return Err(AppError::payload_too_large(
            "source page content is too large",
        ));
    }
    result.status.map_err(|err| {
        AppError::bad_request(format!("source page content could not be decoded: {err}"))
    })?;
    Ok(writer.bytes)
}

fn ensure_decoded_limit(decoded: Vec<u8>, limit: usize) -> AppResult<Vec<u8>> {
    if decoded.len() > limit {
        Err(AppError::payload_too_large(
            "source page content is too large",
        ))
    } else {
        Ok(decoded)
    }
}

fn append_limited(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> AppResult<()> {
    if output
        .len()
        .checked_add(bytes.len())
        .is_none_or(|length| length > limit)
    {
        return Err(AppError::payload_too_large(
            "source page content is too large",
        ));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn unsupported_content_encoding() -> AppError {
    AppError::bad_request("source page uses unsupported multi-stream content encoding")
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > self.limit)
        {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "decoded content limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use lopdf::{dictionary, Object, Stream};

    use super::*;

    #[test]
    fn compressed_content_is_decoded_with_a_hard_limit() {
        let plain = b"q 0 0 10 10 re f";
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(plain).unwrap();
        let stream = Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            encoder.finish().unwrap(),
        );
        assert_eq!(
            decode_page_content_stream(&stream, plain.len()).unwrap(),
            plain
        );

        let expanded = vec![b'q'; 1024];
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&expanded).unwrap();
        let stream = Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            encoder.finish().unwrap(),
        );
        let error = decode_page_content_stream(&stream, 128).unwrap_err();
        assert!(matches!(error, AppError::PayloadTooLarge(_)));
    }

    #[test]
    fn standard_filter_chains_are_decoded() {
        use weezl::{encode::Encoder, BitOrder};

        let plain = b"q 0 0 10 10 re f Q";
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(plain).unwrap();
        let compressed = encoder.finish().unwrap();
        let hex = compressed
            .iter()
            .flat_map(|byte| format!("{byte:02X}").into_bytes())
            .chain(*b">")
            .collect();
        let chained = Stream::new(
            dictionary! {
                "Filter" => vec![
                    Object::Name(b"ASCIIHexDecode".to_vec()),
                    Object::Name(b"FlateDecode".to_vec()),
                ],
                "DecodeParms" => vec![Object::Null, Object::Dictionary(dictionary! { "Predictor" => 1 })],
            },
            hex,
        );
        assert_eq!(
            decode_page_content_stream(&chained, plain.len()).unwrap(),
            plain
        );

        let ascii85 = Stream::new(
            dictionary! { "Filter" => "ASCII85Decode", "DecodeParms" => Object::Null },
            b"9jqo^~>".to_vec(),
        );
        assert_eq!(decode_page_content_stream(&ascii85, 4).unwrap(), b"Man ");

        let encoded_lzw = Encoder::with_tiff_size_switch(BitOrder::Msb, 8)
            .encode(plain)
            .unwrap();
        let lzw = Stream::new(dictionary! { "Filter" => "LZWDecode" }, encoded_lzw);
        assert_eq!(
            decode_page_content_stream(&lzw, plain.len()).unwrap(),
            plain
        );

        let run_length = Stream::new(
            dictionary! { "Filter" => "RunLengthDecode" },
            vec![2, b'a', b'b', b'c', 254, b'd', 128],
        );
        assert_eq!(
            decode_page_content_stream(&run_length, 6).unwrap(),
            b"abcddd"
        );
    }
}
