#[derive(Clone, Default)]
pub(super) struct Bits {
    pub bytes: Vec<u8>,
    pub len: usize,
}

impl Bits {
    pub fn bit(&mut self, bit: bool) {
        if self.len & 7 == 0 {
            self.bytes.push(0);
        }
        if bit {
            *self.bytes.last_mut().unwrap() |= 1 << (7 - (self.len & 7));
        }
        self.len += 1;
    }
    pub fn put(&mut self, value: u32, mut count: usize) {
        while count > 0 {
            let used = self.len & 7;
            if used == 0 {
                self.bytes.push(0);
            }
            let n = count.min(8 - used);
            count -= n;
            *self.bytes.last_mut().unwrap() |=
                (((value >> count) & ((1 << n) - 1)) << (8 - used - n)) as u8;
            self.len += n;
        }
    }
    pub fn ue(&mut self, value: u32) {
        let value = value + 1;
        let n = 31 - value.leading_zeros() as usize;
        self.put(0, n);
        self.put(value, n + 1);
    }
    pub fn se(&mut self, value: i32) {
        self.ue(if value <= 0 {
            (-value as u32) * 2
        } else {
            value as u32 * 2 - 1
        });
    }
    pub fn nal(mut self, header: u8, out: &mut Vec<u8>) {
        self.bit(true);
        while self.len & 7 != 0 {
            self.bit(false);
        }
        out.extend_from_slice(&[0, 0, 0, 1, header]);
        let mut zeros = 0;
        for b in self.bytes {
            if zeros == 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
    }
}

pub(super) fn signed_bits(v: i32) -> u32 {
    let v = if v <= 0 {
        v.unsigned_abs() * 2 + 1
    } else {
        v as u32 * 2
    };
    2 * (31 - v.leading_zeros()) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn put_matches_individual_bits_at_every_alignment() {
        for used in 0..8 {
            for count in 0..=32 {
                for value in [0, 1, u32::MAX, 0xa5c31987] {
                    let mut batch = Bits::default();
                    for _ in 0..used {
                        batch.bit(true);
                    }
                    let mut scalar = batch.clone();
                    batch.put(value, count);
                    for i in (0..count).rev() {
                        scalar.bit((value >> i) & 1 != 0);
                    }
                    assert_eq!(batch.bytes, scalar.bytes);
                    assert_eq!(batch.len, scalar.len);
                }
            }
        }
    }
}
