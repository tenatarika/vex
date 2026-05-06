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
    }
}
