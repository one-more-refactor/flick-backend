//! Engine sanity: weight distribution + slowest/fastest tokens for a text file.
fn main() {
    let path = std::env::args().nth(1).expect("usage: pace <file>");
    let text = std::fs::read_to_string(&path).expect("read file");
    let t = flick_core::Timeline::from_text(&text);
    let n = t.words.len() as f32;
    let mean: f32 = t.words.iter().map(|w| w.2).sum::<f32>() / n;
    let mut sorted: Vec<_> = t.words.iter().collect();
    sorted.sort_by(|a, b| a.2.total_cmp(&b.2));
    let p = |q: f32| sorted[((n - 1.0) * q) as usize].2;
    println!("{path}: {} tokens  mean={mean:.3}  p05={} p50={} p95={} max={}",
        t.word_count, p(0.05), p(0.5), p(0.95), p(1.0));
    print!("fastest: ");
    for w in sorted.iter().take(6) { print!("[{} {}] ", w.0, w.2); }
    println!();
    print!("slowest: ");
    for w in sorted.iter().rev().take(6) { print!("[{} {}] ", w.0, w.2); }
    println!();
    // a mid-document sample of 14 tokens with weights
    let mid = t.words.len() / 2;
    print!("sample:  ");
    for w in &t.words[mid..(mid + 14).min(t.words.len())] { print!("{}·{} ", w.0, w.2); }
    println!("\n");
}
