//! Shared companion pack media decoding.

pub struct Png {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct PcmWave {
    pub rate: u32,
    pub pcm: Vec<u8>,
}

pub fn read_png(bytes: &[u8], custom_size: Option<(u32, u32)>) -> Result<Png, String> {
    let input = std::io::Cursor::new(bytes);
    let mut decoder = if custom_size.is_some() {
        png::Decoder::new_with_limits(
            input,
            png::Limits {
                bytes: 2 * 1024 * 1024,
            },
        )
    } else {
        png::Decoder::new(input)
    };
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let header = decoder
        .read_header_info()
        .map_err(|error| error.to_string())?;
    if custom_size.is_some_and(|size| size != (header.width, header.height)) {
        return Err("unexpected frame dimensions".into());
    }
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    if custom_size.is_some() && reader.info().animation_control.is_some() {
        return Err("use individual PNG frames".into());
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|error| error.to_string())?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(buf.len() / 3 * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|px| [px[0], px[0], px[0], px[1]])
            .collect(),
        png::ColorType::Indexed => return Err("unexpanded indexed color".into()),
    };
    if custom_size.is_some() && !rgba.chunks_exact(4).any(|px| px[3] != 0) {
        return Err("fully transparent frame".into());
    }
    Ok(Png {
        width: info.width,
        height: info.height,
        rgba,
    })
}

pub fn decode_wave(bytes: &[u8]) -> Option<PcmWave> {
    let size = u32_at(bytes, 4)? as usize;
    if bytes.get(..4)? != b"RIFF"
        || bytes.get(8..12)? != b"WAVE"
        || size.checked_add(8)? != bytes.len()
    {
        return None;
    }
    let mut rate = None;
    let mut pcm = None;
    let mut remaining = &bytes[12..];
    while !remaining.is_empty() {
        let size = u32_at(remaining, 4)? as usize;
        let end = 8usize.checked_add(size)?;
        let chunk = remaining.get(8..end)?;
        match remaining.get(..4)? {
            b"fmt " if rate.is_none() => {
                let sample_rate = u32_at(chunk, 4)?;
                if !(8_000..=48_000).contains(&sample_rate)
                    || chunk.get(..4)? != [1, 0, 1, 0]
                    || u32_at(chunk, 8)? != sample_rate * 2
                    || chunk.get(12..16)? != [2, 0, 16, 0]
                {
                    return None;
                }
                rate = Some(sample_rate);
            }
            b"data" if pcm.is_none() => pcm = Some(chunk),
            b"fmt " | b"data" => return None,
            _ => {}
        }
        remaining = remaining.get(end.checked_add(size % 2)?..)?;
    }
    let rate = rate?;
    let pcm = pcm?;
    if pcm.is_empty() || pcm.len() % 2 != 0 || pcm.len() > rate as usize * 2 * 3 {
        return None;
    }
    Some(PcmWave {
        rate,
        pcm: pcm.to_vec(),
    })
}

fn u32_at(bytes: &[u8], start: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(start..start + 4)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::decode_wave;

    #[test]
    fn accepts_short_mono_wave_and_rejects_bad_headers_and_duration() {
        let wave = |samples: usize| {
            let mut bytes = b"RIFF".to_vec();
            bytes.extend_from_slice(&(36 + samples as u32 * 2).to_le_bytes());
            bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
            bytes.extend_from_slice(&16_000u32.to_le_bytes());
            bytes.extend_from_slice(&32_000u32.to_le_bytes());
            bytes.extend_from_slice(b"\x02\0\x10\0data");
            bytes.extend_from_slice(&(samples as u32 * 2).to_le_bytes());
            bytes.resize(44 + samples * 2, 0);
            bytes
        };
        let mut bytes = wave(20);
        assert_eq!(decode_wave(&bytes).unwrap().pcm.len(), 40);
        assert!(decode_wave(&bytes[..bytes.len() - 1]).is_none());
        bytes[22] = 2;
        assert!(decode_wave(&bytes).is_none());
        assert!(decode_wave(&wave(48_001)).is_none());
    }
}
