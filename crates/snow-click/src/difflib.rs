//! `difflib.get_close_matches`, used by Click's "Possible options" hint.

#[allow(clippy::needless_range_loop)]
fn longest_match(
    a: &[char],
    b: &[char],
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    // Straightforward port of SequenceMatcher.find_longest_match without junk.
    let mut besti = alo;
    let mut bestj = blo;
    let mut bestsize = 0;
    let mut j2len: std::collections::HashMap<usize, usize> = Default::default();
    for i in alo..ahi {
        let mut newj2len: std::collections::HashMap<usize, usize> = Default::default();
        for j in blo..bhi {
            if b[j] != a[i] {
                continue;
            }
            let k = j
                .checked_sub(1)
                .and_then(|p| j2len.get(&p))
                .copied()
                .unwrap_or(0)
                + 1;
            newj2len.insert(j, k);
            if k > bestsize {
                besti = i + 1 - k;
                bestj = j + 1 - k;
                bestsize = k;
            }
        }
        j2len = newj2len;
    }
    while besti > alo && bestj > blo && a[besti - 1] == b[bestj - 1] {
        besti -= 1;
        bestj -= 1;
        bestsize += 1;
    }
    while besti + bestsize < ahi
        && bestj + bestsize < bhi
        && a[besti + bestsize] == b[bestj + bestsize]
    {
        bestsize += 1;
    }
    (besti, bestj, bestsize)
}

fn matching_chars(a: &[char], b: &[char]) -> usize {
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut total = 0;
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(a, b, alo, ahi, blo, bhi);
        if k > 0 {
            total += k;
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    total
}

/// `SequenceMatcher(None, a, b).ratio()`.
pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let len = a.len() + b.len();
    if len == 0 {
        return 1.0;
    }
    2.0 * matching_chars(&a, &b) as f64 / len as f64
}

/// `get_close_matches(word, possibilities, n=3, cutoff=0.6)`.
pub fn get_close_matches<'a>(
    word: &str,
    possibilities: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut scored: Vec<(f64, &str)> = Vec::new();
    for x in possibilities {
        // real_quick_ratio and quick_ratio are upper bounds; ratio decides.
        let r = ratio(x, word);
        if r >= 0.6 {
            scored.push((r, x));
        }
    }
    // heapq.nlargest by (score, string).
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(a.1))
    });
    scored
        .into_iter()
        .take(3)
        .map(|(_, s)| s.to_string())
        .collect()
}
