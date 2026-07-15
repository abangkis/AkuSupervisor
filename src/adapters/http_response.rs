#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn parse_response(bytes: &[u8]) -> Result<HttpResponse, String> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response headers were incomplete".to_owned())?;
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "HTTP response headers were not UTF-8".to_owned())?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| "HTTP response status was invalid".to_owned())?;
    let encoded_body = &bytes[(header_end + 4)..];
    let chunked = header.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
        })
    });
    let body = if chunked {
        decode_chunked_body(encoded_body)?
    } else {
        encoded_body.to_vec()
    };
    Ok(HttpResponse { status, body })
}

pub(crate) fn decode_chunked_body(encoded: &[u8]) -> Result<Vec<u8>, String> {
    let mut cursor = 0;
    let mut decoded = Vec::new();
    loop {
        let line_end = encoded[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|offset| cursor + offset)
            .ok_or_else(|| "HTTP chunk size line was incomplete".to_owned())?;
        let size_line = std::str::from_utf8(&encoded[cursor..line_end])
            .map_err(|_| "HTTP chunk size was not ASCII".to_owned())?;
        let size_token = size_line
            .split_once(';')
            .map_or(size_line, |(size, _)| size);
        let size = usize::from_str_radix(size_token.trim(), 16)
            .map_err(|_| "HTTP chunk size was invalid".to_owned())?;
        cursor = line_end + 2;

        if size == 0 {
            let trailers = &encoded[cursor..];
            if trailers == b"\r\n" || trailers.ends_with(b"\r\n\r\n") {
                return Ok(decoded);
            }
            return Err("HTTP chunk trailers were incomplete".to_owned());
        }

        let data_end = cursor
            .checked_add(size)
            .ok_or_else(|| "HTTP chunk size exceeded the response bound".to_owned())?;
        let framing_end = data_end
            .checked_add(2)
            .ok_or_else(|| "HTTP chunk framing exceeded the response bound".to_owned())?;
        if framing_end > encoded.len() || &encoded[data_end..framing_end] != b"\r\n" {
            return Err("HTTP chunk data was incomplete".to_owned());
        }
        decoded.extend_from_slice(&encoded[cursor..data_end]);
        cursor = framing_end;
    }
}
