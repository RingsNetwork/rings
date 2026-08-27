/// Compress equal adjacent iterator values into inclusive index ranges.
pub fn compress_iter<T>(iter: impl Iterator<Item = T>) -> Vec<(T, u64, u64)>
where T: PartialEq {
    let mut result = vec![];
    let mut start = 0u64;
    let mut count = 0u64;
    let mut prev: Option<T> = None;

    for (i, x) in iter.enumerate() {
        match prev {
            Some(p) if p == x => {
                count += 1;
            }
            _ => {
                if let Some(p) = prev {
                    result.push((p, start, start + count - 1));
                }
                start = i as u64;
                count = 1;
            }
        }
        prev = Some(x);
    }

    if let Some(p) = prev {
        result.push((p, start, start + count - 1));
    }

    result
}
