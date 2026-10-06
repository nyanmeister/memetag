//! Bounded, reproducible property checks: no GUI, network, collection, or new dependencies.
use crate::{containers, query, similar, vocab::Vocab, xmp};
use std::collections::{BTreeMap, BTreeSet};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n
    }
    fn text(&mut self) -> String {
        let alphabet: Vec<_> = "aAé猫🦀 :,|()-!*?.<>&\"'/\\\n\t=012".chars().collect();
        (0..self.below(48))
            .map(|_| alphabet[self.below(alphabet.len())])
            .collect()
    }
}

// A deliberately different algorithm from the production greedy matcher.
fn glob_reference(pattern: &str, text: &str) -> bool {
    let text: Vec<_> = text.chars().collect();
    let mut row = vec![false; text.len() + 1];
    row[0] = true;
    for p in pattern.chars() {
        let mut next = vec![false; row.len()];
        next[0] = p == '*' && row[0];
        for j in 1..row.len() {
            next[j] = if p == '*' {
                row[j] || next[j - 1]
            } else {
                row[j - 1] && (p == '?' || p.to_lowercase().eq(text[j - 1].to_lowercase()))
            };
        }
        row = next;
    }
    row[text.len()]
}

#[test]
fn generated_globs_agree_with_dynamic_programming() {
    let mut rng = Rng(0x6123456789abcdef);
    let alphabet = ['a', 'A', 'b', 'é', 'É', '猫', '?', '*'];
    for case in 0..10_000 {
        let pattern: String = (0..rng.below(12)).map(|_| alphabet[rng.below(8)]).collect();
        let text: String = (0..rng.below(12)).map(|_| alphabet[rng.below(6)]).collect();
        assert_eq!(
            query::glob(&pattern, &text),
            glob_reference(&pattern, &text),
            "case {case}: {pattern:?} against {text:?}"
        );
    }
}

#[test]
fn generated_literal_tags_and_completion_ranges_are_unicode_safe() {
    let mut rng = Rng(0x7123456789abcdef);
    for case in 0..2000 {
        let raw = rng.text();
        let tag = raw.trim();
        if !tag.is_empty() {
            assert_eq!(
                query::parse(&query::quote_tag(tag)).unwrap(),
                query::Expr::ExactTag(tag.to_ascii_lowercase()),
                "case {case}: {tag:?}"
            );
        }
        // Carets are character offsets; returned replacement ranges are byte offsets.
        for caret in 0..=raw.chars().count() + 1 {
            if let Some((range, _)) = query::completion(&raw, caret) {
                assert!(
                    range.start <= range.end && range.end <= raw.len(),
                    "case {case}"
                );
                assert!(
                    raw.is_char_boundary(range.start) && raw.is_char_boundary(range.end),
                    "case {case}"
                );
                let mut replaced = raw.clone();
                replaced.replace_range(range, &query::quote_tag("猫, OR 🦀"));
            }
        }
        let _ = query::parse(&raw); // malformed grammar must report errors, not panic
    }
}

#[test]
fn generated_xmp_replacement_roundtrips_text_and_manual_empty_marker() {
    let mut rng = Rng(0x8123456789abcdef);
    let foreign = "<other:rating xmlns:other=\"urn:test\">5 &amp; 6</other:rating>";
    let mut packet = format!(
        "<r:RDF xmlns:r=\"{}\"><r:Description>{foreign}</r:Description></r:RDF>",
        xmp::RDF
    );
    for case in 0..500 {
        let tags: Vec<String> = (0..3)
            .map(|_| rng.text().trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let fields = BTreeMap::from([
            (
                "text".into(),
                if case % 2 == 0 {
                    String::new()
                } else {
                    rng.text()
                },
            ),
            ("textSource".into(), "manual".into()),
            ("id".into(), format!("case-{case}")),
        ]);
        packet = xmp::merge(Some(&packet), &tags, &fields).unwrap();
        let parsed = xmp::read(&packet).unwrap();
        assert_eq!(parsed.tags, tags, "case {case}");
        assert_eq!(parsed.fields, fields, "case {case}");
        assert!(
            packet.contains(foreign),
            "foreign metadata changed in case {case}"
        );
        // Clear tags without losing the deliberately empty, human-reviewed OCR marker.
        let cleared = xmp::read(&xmp::merge(Some(&packet), &[], &fields).unwrap()).unwrap();
        assert!(cleared.tags.is_empty());
        assert_eq!(cleared.fields, fields);
    }
}

#[test]
fn generated_implication_graphs_match_reachability_even_with_cycles() {
    let mut rng = Rng(0x9123456789abcdef);
    for case in 0..500 {
        let n = 12;
        let mut edges = vec![vec![false; n]; n];
        let mut vocab = Vocab::default();
        vocab.aliases.insert("alias".into(), "t0".into());
        for (i, row) in edges.iter_mut().enumerate() {
            for (j, edge) in row.iter_mut().enumerate() {
                *edge = rng.below(5) == 0;
                if *edge {
                    vocab.add_rule(&format!("t{i}"), &format!("t{j}"));
                }
            }
        }
        // Boolean transitive closure provides an independent oracle.
        for k in 0..n {
            for i in 0..n {
                for j in 0..n {
                    edges[i][j] |= edges[i][k] && edges[k][j];
                }
            }
        }
        let expected: BTreeSet<_> = (0..n)
            .filter(|&j| j == 0 || edges[0][j])
            .map(|j| format!("t{j}"))
            .collect();
        assert_eq!(vocab.expand([" ALIAS ".into()]), expected, "case {case}");
        assert_eq!(
            vocab.expand(expected.clone()),
            expected,
            "closure case {case}"
        );
        assert!(!vocab.implies("t0", "t0"));
    }
}

#[test]
fn generated_duplicate_groups_never_chain_or_repeat_members() {
    let mut rng = Rng(0xa123456789abcdef);
    for case in 0..500 {
        let base: Vec<u8> = (0..32).map(|_| rng.next() as u8).collect();
        let mut hashes: Vec<_> = (0..20)
            .map(|_| {
                let mut h = base.clone();
                for _ in 0..rng.below(24) {
                    let bit = rng.below(256);
                    h[bit / 8] ^= 1 << (bit % 8);
                }
                h
            })
            .collect();
        hashes.push(vec![255; 32]); // flat, explicitly excluded even at a permissive threshold
        let max = [0, 8, 20, 256][rng.below(4)];
        let groups = similar::groups(&hashes, max);
        let mut seen = BTreeSet::new();
        let mut last_size = usize::MAX;
        for group in &groups {
            assert!(group.len() >= 2 && group.len() <= last_size, "case {case}");
            last_size = group.len();
            for &i in group {
                assert!(
                    seen.insert(i) && similar::informative(&hashes[i]),
                    "case {case}"
                );
                for &j in group {
                    assert!(
                        similar::distance(&hashes[i], &hashes[j]) <= max,
                        "case {case}: {i}, {j}"
                    );
                }
            }
        }
        // No omitted pair of singletons, nor two whole groups, can still be merged.
        let mut partition = groups;
        partition.extend(
            (0..hashes.len())
                .filter(|i| !seen.contains(i) && similar::informative(&hashes[*i]))
                .map(|i| vec![i]),
        );
        for (i, a) in partition.iter().enumerate() {
            for b in &partition[i + 1..] {
                assert!(
                    a.iter().any(|&x| b
                        .iter()
                        .any(|&y| similar::distance(&hashes[x], &hashes[y]) > max)),
                    "unmerged groups in case {case}"
                );
            }
        }
    }
}

fn iso_file(brand: &[u8; 4]) -> Vec<u8> {
    let mut b = 16u32.to_be_bytes().to_vec();
    b.extend_from_slice(b"ftyp");
    b.extend_from_slice(brand);
    b.extend_from_slice(&[0; 4]);
    b
}

#[test]
fn mp4_and_avif_reject_overflowing_and_truncated_box_sizes() {
    for brand in [b"isom", b"avif"] {
        for size in [0, 7, 8, 15, 17, u64::MAX] {
            let mut b = iso_file(brand);
            b.extend_from_slice(&1u32.to_be_bytes());
            b.extend_from_slice(b"free");
            b.extend_from_slice(&size.to_be_bytes());
            assert!(containers::get_xmp(&b).is_err(), "size {size}");
            assert!(containers::set_xmp(&b, "<x/>").is_err(), "size {size}");
            assert!(containers::strip_xmp(&b).is_err(), "size {size}");
        }
        for tail in 1..16 {
            let mut b = iso_file(brand);
            let large = [0, 0, 0, 1, b'f', b'r', b'e', b'e', 0, 0, 0, 0, 0, 0, 0, 16];
            b.extend_from_slice(&large[..tail]);
            assert!(containers::get_xmp(&b).is_err(), "tail {tail}");
        }
    }
}

#[test]
fn metadata_replacement_preserves_media_across_supported_containers() {
    let img = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8 * 20, y as u8 * 20, 80]));
    let mut seeds = vec![
        include_bytes!("../tests/fixtures/animated.png").to_vec(),
        include_bytes!("../tests/fixtures/animated.webp").to_vec(),
        iso_file(b"isom"),
        iso_file(b"avif"),
        crate::matroska::tests_support::sample_with(40, true, false),
        crate::matroska::tests_support::sample_with(0, false, true),
        b"opaque fallback payload".to_vec(),
    ];
    for format in [
        image::ImageFormat::Png,
        image::ImageFormat::Jpeg,
        image::ImageFormat::Gif,
        image::ImageFormat::WebP,
        image::ImageFormat::Bmp,
    ] {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        seeds.push(out.into_inner());
    }
    for original in seeds {
        let kind = containers::sniff(&original);
        let base = containers::strip_xmp(&original).unwrap();
        let mut current = original;
        for tag in ["cat & dog", "猫 <🦀>", "replacement"] {
            let packet = xmp::merge(None, &[tag.into()], &BTreeMap::new()).unwrap();
            current = containers::set_xmp(&current, &packet).unwrap();
            assert_eq!(
                containers::get_xmp(&current).unwrap().as_deref(),
                Some(packet.as_str()),
                "{kind:?}"
            );
            assert_eq!(containers::strip_xmp(&current).unwrap(), base, "{kind:?}");
            assert_eq!(
                containers::set_xmp(&current, &packet).unwrap(),
                current,
                "{kind:?}"
            );
        }
        let cleared = containers::set_xmp(&current, "").unwrap();
        assert_eq!(containers::get_xmp(&cleared).unwrap(), None, "{kind:?}");
        assert_eq!(containers::strip_xmp(&cleared).unwrap(), base, "{kind:?}");
    }
}
