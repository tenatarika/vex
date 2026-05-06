use std::path::PathBuf;
use tempfile::TempDir;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

#[test]
fn index_and_search_roundtrip() {
    let fixtures = fixtures_dir();
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    // Parse fixtures
    let mut all_parsed = Vec::new();
    for entry in std::fs::read_dir(&fixtures).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang = match vex::parse::language::Language::from_extension(ext) {
            Some(l) => l,
            None => continue,
        };
        let content = std::fs::read_to_string(&path).unwrap();
        let rel = path.file_name().unwrap().to_string_lossy().to_string();
        if let Ok(parsed) = vex::parse::parse_file(&rel, &content, lang) {
            all_parsed.push(parsed);
        }
    }

    assert!(!all_parsed.is_empty(), "should parse at least one fixture");

    let total_symbols: usize = all_parsed.iter().map(|f| f.symbols.len()).sum();
    assert!(total_symbols > 0, "should extract at least one symbol");

    // Write index
    vex::store::writer::write_index(&all_parsed, &index_path).unwrap();
    assert!(index_path.exists());

    // Read index
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    assert_eq!(reader.symbol_count(), total_symbols);

    // Build inverted index and search
    let inverted = vex::store::inverted::InvertedIndex::from_reader(&reader);

    // Search for Rust fixture symbols
    let results = vex::search::structural::search(&reader, &inverted, "PaymentService", 10);
    assert!(
        !results.is_empty(),
        "should find PaymentService from sample.rs"
    );
    assert_eq!(results[0].name, "PaymentService");

    // Search for Go fixture symbols
    let results = vex::search::structural::search(&reader, &inverted, "InvoiceService", 10);
    assert!(
        !results.is_empty(),
        "should find InvoiceService from sample.go"
    );

    // Search for Python fixture symbols
    let results = vex::search::structural::search(&reader, &inverted, "UserRepository", 10);
    assert!(
        !results.is_empty(),
        "should find UserRepository from sample.py"
    );

    // Prefix search: "Payment" should find PaymentService and PaymentGateway
    let results = vex::search::structural::search(&reader, &inverted, "Payment", 10);
    assert!(
        results.len() >= 2,
        "prefix 'Payment' should match multiple symbols"
    );

    // CamelCase sub-token search: "invoice" should find InvoiceService/InvoiceRepository
    let results = vex::search::structural::search(&reader, &inverted, "invoice", 10);
    assert!(
        !results.is_empty(),
        "sub-token 'invoice' should match via CamelCase split"
    );
}

#[test]
fn search_nonexistent_returns_empty() {
    let fixtures = fixtures_dir();
    let tmp = TempDir::new().unwrap();
    let index_path = tmp.path().join("index.vex");

    let content = std::fs::read_to_string(fixtures.join("sample.rs")).unwrap();
    let parsed =
        vex::parse::parse_file("sample.rs", &content, vex::parse::language::Language::Rust)
            .unwrap();

    vex::store::writer::write_index(&[parsed], &index_path).unwrap();
    let reader = vex::store::reader::IndexReader::open(&index_path).unwrap();
    let inverted = vex::store::inverted::InvertedIndex::from_reader(&reader);

    let results = vex::search::structural::search(&reader, &inverted, "NonExistentSymbol", 10);
    assert!(results.is_empty());
}
