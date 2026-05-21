use crate::search::explain::Explanation;
use crate::search::similar::SimilarMatch;
use crate::search::SearchResult;

pub fn print_results(results: &[SearchResult], format: &super::args::OutputFormat) {
    match format {
        super::args::OutputFormat::Json => {
            let json = serde_json::to_string_pretty(results).unwrap_or_default();
            println!("{json}");
        }
        super::args::OutputFormat::Text => {
            for r in results {
                println!(
                    "{kind:<12} {name:<40} {path}:{line}",
                    kind = r.kind,
                    name = r.name,
                    path = r.path,
                    line = r.line,
                );
                if let Some(sig) = &r.signature {
                    println!("             {sig}");
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for r in results {
                let kind = compact_kind(&r.kind);
                print!(
                    "{kind} {name} {path}:{line}",
                    name = r.name,
                    path = r.path,
                    line = r.line
                );
                if let Some(sig) = &r.signature {
                    print!(" {sig}");
                }
                println!();
            }
        }
    }
}

pub fn print_similar(
    matches: &[SimilarMatch],
    target: &str,
    explanations: Option<&[Explanation]>,
    format: &super::args::OutputFormat,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = matches
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let mut obj = serde_json::json!({
                        "name": m.name,
                        "kind": m.kind,
                        "path": m.path,
                        "line": m.line,
                        "similarity": m.similarity,
                        "signature": m.signature,
                    });
                    if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                        obj["explanation"] = explanation_json(ex);
                    }
                    obj
                })
                .collect();
            // unwrap: serializing simple JSON values cannot fail
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        super::args::OutputFormat::Text => {
            if matches.is_empty() {
                println!("No similar symbols found for \"{target}\"");
                return;
            }
            println!("Similar to \"{target}\":");
            for (i, m) in matches.iter().enumerate() {
                println!(
                    " {sim:>5.3}  {kind:<10} {name:<40} {path}:{line}",
                    sim = m.similarity,
                    kind = m.kind,
                    name = m.name,
                    path = m.path,
                    line = m.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_text(ex);
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for (i, m) in matches.iter().enumerate() {
                let kind = compact_kind(&m.kind);
                print!(
                    "{sim:.3} {kind} {name} {path}:{line}",
                    sim = m.similarity,
                    name = m.name,
                    path = m.path,
                    line = m.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_compact(ex);
                }
                println!();
            }
        }
    }
}

pub fn print_duplicates(
    pairs: &[(SimilarMatch, SimilarMatch)],
    explanations: Option<&[Explanation]>,
    format: &super::args::OutputFormat,
) {
    match format {
        super::args::OutputFormat::Json => {
            let json: Vec<serde_json::Value> = pairs
                .iter()
                .enumerate()
                .map(|(i, (a, b))| {
                    let mut obj = serde_json::json!({
                        "similarity": a.similarity,
                        "a": {
                            "name": a.name,
                            "kind": a.kind,
                            "path": a.path,
                            "line": a.line,
                        },
                        "b": {
                            "name": b.name,
                            "kind": b.kind,
                            "path": b.path,
                            "line": b.line,
                        },
                    });
                    if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                        obj["explanation"] = explanation_json(ex);
                    }
                    obj
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        super::args::OutputFormat::Text => {
            if pairs.is_empty() {
                println!("No duplicates found");
                return;
            }
            for (i, (a, b)) in pairs.iter().enumerate() {
                println!(
                    " {sim:>5.3}  {name}  {path}:{line}",
                    sim = a.similarity,
                    name = a.name,
                    path = a.path,
                    line = a.line
                );
                println!(
                    "         {name}  {path}:{line}",
                    name = b.name,
                    path = b.path,
                    line = b.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_text(ex);
                }
            }
        }
        super::args::OutputFormat::Compact => {
            for (i, (a, b)) in pairs.iter().enumerate() {
                let ak = compact_kind(&a.kind);
                let bk = compact_kind(&b.kind);
                print!(
                    "{sim:.3} {ak} {an} {ap}:{al} | {bk} {bn} {bp}:{bl}",
                    sim = a.similarity,
                    an = a.name,
                    ap = a.path,
                    al = a.line,
                    bn = b.name,
                    bp = b.path,
                    bl = b.line
                );
                if let Some(ex) = explanations.and_then(|e| e.get(i)) {
                    print_explanation_compact(ex);
                }
                println!();
            }
        }
    }
}

fn explanation_json(ex: &Explanation) -> serde_json::Value {
    serde_json::json!({
        "identifier_jaccard": ex.identifier_jaccard,
        "diff_added": ex.added,
        "diff_removed": ex.removed,
        "diff": ex.diff,
    })
}

fn print_explanation_text(ex: &Explanation) {
    println!(
        "         jaccard {jac:.2}  diff +{add} -{rem}",
        jac = ex.identifier_jaccard,
        add = ex.added,
        rem = ex.removed,
    );
    for line in ex.diff.lines() {
        println!("           {line}");
    }
}

fn print_explanation_compact(ex: &Explanation) {
    // Compact format already uses ` | ` as the a/b separator in
    // `print_duplicates`; using `;` here keeps `--explain --format compact`
    // unambiguous for ` | `-splitting consumers.
    print!(
        " ; jac {jac:.2} +{add} -{rem}",
        jac = ex.identifier_jaccard,
        add = ex.added,
        rem = ex.removed,
    );
}

/// Single-char kind code for compact output.
fn compact_kind(kind: &str) -> char {
    match kind {
        "function" => 'F',
        "method" => 'M',
        "struct" => 'S',
        "class" => 'C',
        "interface" => 'I',
        "trait" => 'T',
        "enum" => 'E',
        "type_alias" => 'A',
        "impl" => 'i',
        "constant" => 'K',
        "property" => 'P',
        "package" => 'G',
        "heading" => 'H',
        _ => '?',
    }
}
