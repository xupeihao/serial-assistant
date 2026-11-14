use encoding_rs::{Decoder, Encoding};

const INVALID_HEX: &str = "请输入成对的十六进制数字，例如 A1 B2 0D 0A。";

pub fn encode(
    input: &str,
    hex: bool,
    encoding: &'static Encoding,
    ending: &str,
) -> Result<Vec<u8>, &'static str> {
    if hex {
        let mut output = Vec::with_capacity(input.len() / 2);
        for token in input.split_ascii_whitespace() {
            let (pairs, remainder) = token.as_bytes().as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(INVALID_HEX);
            }
            for &[high, low] in pairs {
                let high = (high as char).to_digit(16);
                let low = (low as char).to_digit(16);
                let (Some(high), Some(low)) = (high, low) else {
                    return Err(INVALID_HEX);
                };
                output.push((high * 16 + low) as u8);
            }
        }
        return Ok(output);
    }

    let input = format!("{input}{ending}");
    let (bytes, _, errors) = encoding.encode(&input);
    if errors {
        return Err("输入含有当前编码无法表示的字符。");
    }
    Ok(bytes.into_owned())
}

pub struct ReceiveFormat {
    decoder: Decoder,
    hex: bool,
    column: usize,
    previous_cr: bool,
}

impl ReceiveFormat {
    pub fn new(hex: bool, encoding: &'static Encoding) -> Self {
        Self {
            decoder: encoding.new_decoder_without_bom_handling(),
            hex,
            column: 0,
            previous_cr: false,
        }
    }

    pub fn format_batch(&mut self, bytes: &[u8], complete: bool) -> String {
        self.column = 0;
        if self.hex {
            return self.hex_text(bytes);
        }

        let mut text =
            String::with_capacity(self.decoder.max_utf8_buffer_length(bytes.len()).unwrap());
        let (_, read, _) = self.decoder.decode_to_string(bytes, &mut text, complete);
        debug_assert_eq!(read, bytes.len());
        let mut display = String::with_capacity(text.len());
        for ch in text.chars() {
            match ch {
                '\n' if self.previous_cr => self.previous_cr = false,
                '\r' | '\n' => {
                    display.push_str("\r\n");
                    self.column = 0;
                    self.previous_cr = ch == '\r';
                }
                _ => {
                    if self.column == 1024 {
                        display.push_str("\r\n");
                        self.column = 0;
                    }
                    display.push(if ch == '\0' { '␀' } else { ch });
                    self.column += 1;
                    self.previous_cr = false;
                }
            }
        }
        display
    }

    fn hex_text(&mut self, bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut display = String::with_capacity(bytes.len() * 3);
        for &byte in bytes {
            if self.column == 16 {
                display.push_str("\r\n");
                self.column = 0;
            } else if self.column != 0 {
                display.push(' ');
            }
            display.push(HEX[(byte >> 4) as usize] as char);
            display.push(HEX[(byte & 15) as usize] as char);
            self.column += 1;
        }
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{GBK, UTF_8};

    #[test]
    fn send_preserves_raw_hex_and_text_line_endings() {
        assert_eq!(
            encode("A1b2 00\r\n0d0a", true, UTF_8, "\r\n"),
            Ok(vec![0xa1, 0xb2, 0, 13, 10])
        );
        assert_eq!(
            encode("中", false, GBK, "\r\n"),
            Ok(vec![0xd6, 0xd0, 13, 10])
        );
        for invalid in ["A B", "0xAF", "Z1", "中", "C"] {
            assert!(encode(invalid, true, UTF_8, "").is_err(), "{invalid}");
        }
        assert!(encode("🙂", false, GBK, "").is_err());
    }

    #[test]
    fn receive_preserves_characters_and_crlf_across_reads() {
        let mut utf8 = ReceiveFormat::new(false, UTF_8);
        assert_eq!(utf8.format_batch(&[0xe4, 0xb8], false), "");
        assert_eq!(utf8.format_batch(&[0xad, b'\r'], false), "中\r\n");
        assert_eq!(utf8.format_batch(b"\nB\r\r\n\0", false), "B\r\n\r\n␀");
        let mut gbk = ReceiveFormat::new(false, GBK);
        assert_eq!(gbk.format_batch(&[0xd6], false), "");
        assert_eq!(gbk.format_batch(&[0xd0], false), "中");
    }

    #[test]
    fn receive_wraps_long_streams_and_keeps_hex_byte_boundaries() {
        let mut hex = ReceiveFormat::new(true, UTF_8);
        assert_eq!(
            hex.format_batch(&[0x0a; 17], false),
            format!("{}\r\n0A", ["0A"; 16].join(" "))
        );
        assert_eq!(hex.format_batch(&[0, 0xff], false), "00 FF");
        let mut text = ReceiveFormat::new(false, UTF_8);
        assert_eq!(text.format_batch(&[b'A'; 1025], false).len(), 1027);
        assert_eq!(text.format_batch(b"B", false), "B");
    }
}
