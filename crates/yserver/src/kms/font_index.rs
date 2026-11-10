//! X core font name matching, after libXfont2's fontfile FPE
//! (`src/fontfile/fontdir.c`, `fontfile.c`).
//!
//! Names are lowered once, when a font path element is read, and a
//! pattern once per request; matching is then a byte comparison with no
//! allocation. Lowering is libXfont's `ISOLatin1ToLower` on bytes, so `?`
//! matches one byte, as on Xorg.

/// libXfont's `ISOLatin1ToLower`: ASCII and the Latin-1 capitals.
pub(crate) fn latin1_lower(b: u8) -> u8 {
    match b {
        b'A'..=b'Z' | 0xC0..=0xD6 | 0xD8..=0xDE => b + 0x20,
        _ => b,
    }
}

/// A font name as the matcher sees it: lowered bytes and its dash count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoweredName {
    pub(crate) bytes: Box<[u8]>,
    pub(crate) dashes: u32,
}

impl LoweredName {
    pub(crate) fn new(name: &str) -> Self {
        let bytes: Box<[u8]> = name.bytes().map(latin1_lower).collect();
        let dashes = count_dashes(&bytes);
        Self { bytes, dashes }
    }
}

fn count_dashes(b: &[u8]) -> u32 {
    u32::try_from(b.iter().filter(|&&c| c == b'-').count()).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Star,
    Any,
    Lit(u8),
}

/// A client's font name pattern, lowered and tokenised once.
///
/// Semantics are Xorg's `PatternMatch`, including its quirk that the
/// character right after a `*` is compared literally: `*?` needs a `?` in
/// the name and `**` a `*`.
#[derive(Debug, Clone)]
pub(crate) struct FontPattern {
    toks: Vec<Tok>,
    dashes: u32,
    lowered: Box<[u8]>,
    /// Holds a `*` or `?` (`SetupWildMatch`'s `firstWild`).
    wild: bool,
    /// Bytes before the first wildcard or digit: every match starts with
    /// them, so a sorted table holds its matches in one range.
    prefix: usize,
}

impl FontPattern {
    pub(crate) fn new(pattern: &str) -> Self {
        let lowered: Vec<u8> = pattern.bytes().map(latin1_lower).collect();
        let mut toks = Vec::with_capacity(lowered.len());
        let mut it = lowered.iter().copied();
        while let Some(c) = it.next() {
            match c {
                b'*' => {
                    toks.push(Tok::Star);
                    if let Some(next) = it.next() {
                        toks.push(Tok::Lit(next));
                    }
                }
                b'?' => toks.push(Tok::Any),
                _ => toks.push(Tok::Lit(c)),
            }
        }
        let first_wild = lowered.iter().position(|&c| c == b'*' || c == b'?');
        let first_digit = lowered.iter().position(u8::is_ascii_digit);
        let prefix = match (first_wild, first_digit) {
            (Some(w), Some(d)) => w.min(d),
            (Some(w), None) => w,
            (None, _) => lowered.len(),
        };
        Self {
            toks,
            dashes: count_dashes(&lowered),
            wild: first_wild.is_some(),
            prefix,
            lowered: lowered.into_boxed_slice(),
        }
    }

    /// Whether the lowered `name` matches. Single pass with one backtrack
    /// point (the last `*`), so O(|pattern| x |name|) — the recursion in
    /// `PatternMatch` is exponential in the star count (#155).
    pub(crate) fn matches(&self, name: &LoweredName) -> bool {
        // Every `-` in the pattern is a literal that needs its own `-`.
        if name.dashes < self.dashes {
            return false;
        }
        let (t, n) = (&self.toks, &name.bytes[..]);
        let (mut pi, mut ni) = (0usize, 0usize);
        let mut star: Option<usize> = None;
        let mut star_ni = 0usize;
        while ni < n.len() {
            match t.get(pi) {
                Some(Tok::Any) => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                Some(&Tok::Lit(c)) if c == n[ni] => {
                    pi += 1;
                    ni += 1;
                    continue;
                }
                Some(Tok::Star) => {
                    star = Some(pi);
                    star_ni = ni;
                    pi += 1;
                    continue;
                }
                _ => {}
            }
            let Some(sp) = star else {
                return false;
            };
            pi = sp + 1;
            star_ni += 1;
            ni = star_ni;
        }
        while t.get(pi) == Some(&Tok::Star) {
            pi += 1;
        }
        pi == t.len()
    }
}

/// libXfont's `strcmpn`: `strcmp`, except that runs of digits compare as
/// numbers ("iso8859-2" < "iso8859-10"). The order a directory lists in.
pub(crate) fn strcmpn(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let mut predigits = false;
    let mut i = 0;
    loop {
        let (c1, c2) = (at(a, i), at(b, i));
        if c1 == 0 && c2 == 0 {
            return Ordering::Equal;
        }
        let digits = c1.is_ascii_digit() && c2.is_ascii_digit();
        if digits && !predigits {
            let mut j = i;
            while at(a, j).is_ascii_digit() && at(b, j).is_ascii_digit() {
                j += 1;
            }
            match (at(a, j).is_ascii_digit(), at(b, j).is_ascii_digit()) {
                (false, true) => return Ordering::Less,
                (true, false) => return Ordering::Greater,
                _ => {}
            }
        }
        match c1.cmp(&c2) {
            Ordering::Equal => {}
            o => return o,
        }
        predigits = digits;
        i += 1;
    }
}

/// One name of a [`FontTable`].
#[derive(Debug, Clone)]
pub(crate) struct TableEntry<T> {
    pub(crate) key: LoweredName,
    /// The lowered name as a reply carries it (libXfont lowers the names
    /// it reads).
    pub(crate) name: Box<str>,
    pub(crate) value: T,
}

/// A libXfont font table: names sorted with [`strcmpn`], looked up
/// exactly by hash and by pattern within the range its literal prefix
/// selects (`SetupWildMatch` + `PatternMatch`).
#[derive(Debug, Clone)]
pub(crate) struct FontTable<T> {
    entries: Vec<TableEntry<T>>,
    exact: std::collections::HashMap<Box<[u8]>, usize>,
}

impl<T> Default for FontTable<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            exact: std::collections::HashMap::new(),
        }
    }
}

impl<T> FontTable<T> {
    pub(crate) fn new(items: impl IntoIterator<Item = (String, T)>) -> Self {
        let mut entries: Vec<TableEntry<T>> = items
            .into_iter()
            .map(|(name, value)| {
                let key = LoweredName::new(&name);
                let name = String::from_utf8_lossy(&key.bytes).into();
                TableEntry { key, name, value }
            })
            .collect();
        entries.sort_by(|a, b| strcmpn(&a.key.bytes, &b.key.bytes));
        let mut exact = std::collections::HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            exact.entry(e.key.bytes.clone()).or_insert(i);
        }
        Self { entries, exact }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entries `pattern` matches, in table order. A pattern without
    /// wildcards names at most one entry, even when the table holds the
    /// name twice.
    pub(crate) fn matching<'a>(
        &'a self,
        pattern: &'a FontPattern,
    ) -> impl Iterator<Item = &'a TableEntry<T>> + 'a {
        let range = if pattern.wild {
            let prefix = &pattern.lowered[..pattern.prefix];
            let cut = |e: &TableEntry<T>| {
                let k = &e.key.bytes;
                k[..k.len().min(prefix.len())].cmp(prefix)
            };
            let lo = self
                .entries
                .partition_point(|e| cut(e) == std::cmp::Ordering::Less);
            let hi =
                lo + self.entries[lo..].partition_point(|e| cut(e) == std::cmp::Ordering::Equal);
            lo..hi
        } else {
            match self.exact.get(&pattern.lowered) {
                Some(&i) => i..i + 1,
                None => 0..0,
            }
        };
        self.entries[range]
            .iter()
            .filter(move |e| !pattern.wild || pattern.matches(&e.key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, n: &str) -> bool {
        FontPattern::new(p).matches(&LoweredName::new(n))
    }

    #[test]
    fn latin1_lowering_is_libxfonts() {
        assert_eq!(latin1_lower(b'Q'), b'q');
        assert_eq!(latin1_lower(0xC0), 0xE0);
        assert_eq!(latin1_lower(0xD7), 0xD7); // multiplication sign
        assert_eq!(latin1_lower(0xDE), 0xFE);
        assert_eq!(latin1_lower(0xDF), 0xDF);
        assert_eq!(latin1_lower(b'-'), b'-');
    }

    #[test]
    fn glob() {
        assert!(m("xtfont*", "xtfont0"));
        assert!(m("XTFONT0", "xtfont0"));
        assert!(m("xtfont0", "XTFONT0"));
        assert!(m(
            "-vsw-*-bold-r-*",
            "-vsw-testfont-bold-r-normal--13-130-75-75-m-70-iso8859-1"
        ));
        assert!(!m("xtfont?", "xtfont"));
        assert!(!m("nope", "xtfont0"));
        assert!(m("abc*", "abc"));
        assert!(m("*", ""));
        assert!(m("", ""));
        assert!(!m("", "a"));
        assert!(!m("?", ""));
        assert!(!m("abc", "abcd"));
        assert!(m("*b", "abab"));
        assert!(m("*ab", "aab"));
        assert!(m("a*b*c", "axxbyyc"));
        assert!(!m("*ab", "aba"));
        let name = "-misc-fixed-medium-r-normal--20-200-75-75-c-100-iso8859-1";
        assert!(m("-*-*-*-*-*-*-*-*-*-*-*-*-iso8859-1", name));
        assert!(!m("-*-*-*-*-*-*-*-*-*-*-*-*-iso10646-1", name));
        assert!(m("-*-*-*-*-*-*-*-*-*-*-*-*-*-*", name));
    }

    /// `PatternMatch` takes the character after a `*` literally (Xorg
    /// 21.1, measured in the font-list vng scenario).
    #[test]
    fn star_takes_the_next_character_literally() {
        assert!(!m("5x7**", "5x7"));
        assert!(m("5x7**", "5x7*"));
        assert!(!m("*?x7", "5x7"));
        assert!(m("*?x7", "a?x7"));
        assert!(m("5?7", "5x7"));
        assert!(m("5x7*", "5x7"));
    }

    #[test]
    fn strcmpn_orders_digit_runs_as_numbers() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(strcmpn(b"iso8859-2", b"iso8859-10"), Less);
        assert_eq!(strcmpn(b"iso8859-10", b"iso10646-1"), Less);
        assert_eq!(strcmpn(b"-9-", b"-10-"), Less);
        assert_eq!(strcmpn(b"a", b"a"), Equal);
        assert_eq!(strcmpn(b"ab", b"a"), Greater);
        assert_eq!(strcmpn(b"-a", b"1"), Less);
    }

    /// Order and range lookup: names come back lowered, in `strcmpn`
    /// order, whatever the input order; a prefix range holds every match.
    #[test]
    fn table_lists_in_strcmpn_order() {
        let t = FontTable::new(
            [
                "-Misc-Fixed-Medium-R-Normal--13-120-75-75-C-70-ISO8859-10",
                "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-2",
                "-misc-fixed-bold-r-normal--13-120-75-75-c-70-iso8859-1",
                "fixed",
                "6x13",
                "-adobe-courier-bold-r-normal--10-100-75-75-m-60-iso8859-1",
            ]
            .map(|n| (n.to_string(), ())),
        );
        let names = |p: &str| -> Vec<String> {
            let p = FontPattern::new(p);
            t.matching(&p).map(|e| e.name.to_string()).collect()
        };
        assert_eq!(
            names("*"),
            [
                "-adobe-courier-bold-r-normal--10-100-75-75-m-60-iso8859-1",
                "-misc-fixed-bold-r-normal--13-120-75-75-c-70-iso8859-1",
                "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-2",
                "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-10",
                "6x13",
                "fixed",
            ]
        );
        assert_eq!(
            names("-misc-fixed-medium-*"),
            [
                "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-2",
                "-misc-fixed-medium-r-normal--13-120-75-75-c-70-iso8859-10",
            ]
        );
        assert_eq!(names("FIXED"), ["fixed"]);
        assert_eq!(names("6x1?"), ["6x13"]);
        assert!(names("fixe").is_empty());
        assert!(names("-zzz*").is_empty());
    }

    /// `?` is one byte: a two-byte UTF-8 character needs two.
    #[test]
    fn any_is_one_byte() {
        assert!(!m("?", "é"));
        assert!(m("??", "é"));
        assert!(m("*é*", "xéy"));
    }

    /// The worst case for #155: an all-wildcard XLFD whose literal tail
    /// fails, so every star split is tried.
    #[test]
    fn glob_is_not_exponential() {
        let p = FontPattern::new("-*-*-*-*-*-*-*-*-*-*-*-*-iso8859-1");
        let n = LoweredName::new("-misc-fixed-medium-r-normal--20-200-75-75-c-100-iso10646-1");
        let start = std::time::Instant::now();
        for _ in 0..5000 {
            assert!(!p.matches(&n));
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
    }
}
