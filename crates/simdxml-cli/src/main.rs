use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

const USAGE: &str = "\
sxq — fast XML/XPath query tool, powered by SIMD

Usage: sxq [OPTIONS] <XPATH> [FILE...]

Arguments:
  <XPATH>     XPath 1.0 expression
  [FILE...]   XML files (reads stdin if omitted, - for explicit stdin)

Options:
  -r          Output raw XML fragments instead of text content
  -c          Print only the count of matching nodes
  -j          Output results as a JSON array
  -l          Print only filenames that contain matches
  -0          Separate output with NUL instead of newline
  -W          Include whitespace-only results (stripped by default)
  -t N        Number of threads for parallel batch processing
  -H          Suppress filename headers in multi-file output
  -h, --help  Print this help

Subcommands:
  info        Show structural index statistics for XML files

Examples:
  sxq '//title' book.xml
  sxq '//claim[@type=\"independent\"]' patents/*.xml
  curl -s https://example.com/feed.xml | sxq '//item/title'
  sxq -c '//record' huge.xml
  sxq 'count(//claim)' patent.xml";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        eprintln!("{USAGE}");
        return if args.is_empty() { ExitCode::from(2) } else { ExitCode::SUCCESS };
    }

    if args[0] == "--version" {
        println!("sxq {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if args[0] == "info" {
        return match run_info(&args[1..]) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => { eprintln!("error: {e}"); ExitCode::from(2) }
        };
    }

    // Parse flags
    let mut raw = false;
    let mut count = false;
    let mut json = false;
    let mut files_with_matches = false;
    let mut null_sep = false;
    let mut whitespace = false;
    let mut no_filename = false;
    let mut threads: Option<usize> = None;
    let mut positional: Vec<&str> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-r" => raw = true,
            "-c" => count = true,
            "-j" => json = true,
            "-l" => files_with_matches = true,
            "-0" => null_sep = true,
            "-W" => whitespace = true,
            "-H" => no_filename = true,
            "-t" => {
                i += 1;
                threads = Some(args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1));
            }
            _ => positional.push(arg),
        }
        i += 1;
    }

    if positional.is_empty() {
        eprintln!("error: missing XPath expression\n\n{USAGE}");
        return ExitCode::from(2);
    }

    let xpath = positional[0];
    let files = &positional[1..];

    match run_query(xpath, files, raw, count, json, files_with_matches, null_sep, whitespace, no_filename, threads) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => { eprintln!("error: {e}"); ExitCode::from(2) }
    }
}

fn run_query(
    xpath: &str,
    files: &[&str],
    raw: bool,
    count: bool,
    json: bool,
    files_with_matches: bool,
    null_sep: bool,
    whitespace: bool,
    no_filename: bool,
    threads: Option<usize>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let compiled = simdxml::CompiledXPath::compile(xpath)?;
    let _ = &compiled; // validate early

    let sources = gather_sources(files)?;
    let multi = sources.len() > 1;
    let show_filename = multi && !no_filename;
    let sep = if null_sep { "\0" } else { "\n" };
    let threads = threads.unwrap_or_else(|| std::thread::available_parallelism()
        .map(|n| n.get()).unwrap_or(1));

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    // Batch path for multi-file (compiled XPath reused across all files)
    if multi && !raw && !files_with_matches {
        return run_batch(&compiled, &sources, show_filename, sep, &mut out, threads,
                         count, json, whitespace);
    }

    let mut any_match = false;
    let mut json_all: Vec<Vec<String>> = Vec::new();

    for source in &sources {
        let data = source.read_bytes()?;
        let name = source.name();

        let mut index = simdxml::parse(&data)?;
        let result = index.eval(xpath)?;

        match result {
            simdxml::XPathResult::NodeSet(ref nodes) => {
                if count {
                    let n = nodes.len();
                    if n > 0 { any_match = true; }
                    if show_filename { writeln!(out, "{name}:{n}")?; }
                    else { writeln!(out, "{n}")?; }
                    continue;
                }

                if files_with_matches {
                    if !nodes.is_empty() {
                        any_match = true;
                        writeln!(out, "{name}")?;
                    }
                    continue;
                }

                if raw {
                    index.ensure_indices(); // close_map needed for raw_xml
                    // Extract raw XML directly from already-evaluated nodes
                    let fragments: Vec<&str> = nodes.iter().map(|node| match *node {
                        simdxml::xpath::XPathNode::Element(idx) => index.raw_xml(idx),
                        simdxml::xpath::XPathNode::Text(idx) => index.text_by_index(idx),
                        simdxml::xpath::XPathNode::Attribute(tag_idx, _)
                        | simdxml::xpath::XPathNode::Namespace(tag_idx, _) => index.raw_tag(tag_idx),
                    }).collect();
                    let fragments: Vec<&str> = if whitespace { fragments }
                        else { fragments.into_iter().filter(|s| !s.trim().is_empty()).collect() };
                    if !fragments.is_empty() {
                        any_match = true;
                        if show_filename { writeln!(out, "{name}")?; }
                    }
                    for f in &fragments { write!(out, "{f}{sep}")?; }
                    continue;
                }

                // Build CSR indices for fast text child lookup
                index.ensure_indices();
                let texts = simdxml::xpath::extract_text(&index, nodes)?;
                let texts: Vec<&str> = if whitespace { texts }
                    else { texts.into_iter().filter(|s| !s.trim().is_empty()).collect() };

                if !texts.is_empty() { any_match = true; }

                if json {
                    let owned: Vec<String> = texts.iter()
                        .map(|s| simdxml::XmlIndex::decode_entities(s).into_owned()).collect();
                    if multi { json_all.push(owned); }
                    else { write_json(&mut out, &owned)?; }
                    continue;
                }

                if show_filename && !texts.is_empty() { writeln!(out, "{name}")?; }
                for text in &texts {
                    let decoded = simdxml::XmlIndex::decode_entities(text);
                    write!(out, "{decoded}{sep}")?;
                }
            }
            simdxml::XPathResult::String(ref s) => {
                any_match = !s.is_empty();
                if json {
                    let items = vec![s.clone()];
                    if multi { json_all.push(items); } else { write_json(&mut out, &items)?; }
                } else if count {
                    if show_filename { writeln!(out, "{name}:1")?; }
                    else { writeln!(out, "1")?; }
                } else {
                    if show_filename { writeln!(out, "{name}")?; }
                    write!(out, "{s}{sep}")?;
                }
            }
            simdxml::XPathResult::Number(_) | simdxml::XPathResult::Boolean(_) => {
                if let simdxml::XPathResult::Boolean(b) = result { any_match = b; }
                else { any_match = true; }
                let formatted = result.to_display_string(&index);
                if json {
                    if multi { json_all.push(vec![formatted.clone()]); }
                    else { writeln!(out, "{formatted}")?; }
                } else {
                    if show_filename { writeln!(out, "{name}")?; }
                    write!(out, "{formatted}{sep}")?;
                }
            }
        }
    }

    if json && multi {
        let flat: Vec<String> = json_all.into_iter().flatten().collect();
        write_json(&mut out, &flat)?;
    }

    out.flush()?;
    Ok(any_match)
}

fn run_batch(
    compiled: &simdxml::CompiledXPath,
    sources: &[Source],
    show_filename: bool,
    sep: &str,
    out: &mut impl Write,
    threads: usize,
    count: bool,
    json: bool,
    whitespace: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut names: Vec<&str> = Vec::with_capacity(sources.len());
    let mut all_data: Vec<Vec<u8>> = Vec::with_capacity(sources.len());
    for source in sources {
        all_data.push(source.read_bytes()?);
        names.push(source.name());
    }
    let doc_refs: Vec<&[u8]> = all_data.iter().map(|d| d.as_slice()).collect();

    if count {
        let counts = simdxml::batch::count_batch(&doc_refs, compiled)?;
        let mut any = false;
        for (i, c) in counts.iter().enumerate() {
            if *c > 0 { any = true; }
            if show_filename { writeln!(out, "{}:{c}", names[i])?; }
            else { writeln!(out, "{c}")?; }
        }
        out.flush()?;
        return Ok(any);
    }

    let batch = simdxml::batch::eval_batch_parallel(&doc_refs, compiled, threads)?;
    let mut any = false;
    let mut json_all: Vec<Vec<String>> = Vec::new();

    for (i, results) in batch.iter().enumerate() {
        let results: Vec<&str> = if whitespace {
            results.iter().map(|s| s.as_str()).collect()
        } else {
            results.iter().map(|s| s.as_str()).filter(|s| !s.trim().is_empty()).collect()
        };
        if !results.is_empty() { any = true; }

        if json {
            json_all.push(results.iter()
                .map(|s| simdxml::XmlIndex::decode_entities(s).into_owned()).collect());
            continue;
        }
        if show_filename && !results.is_empty() { writeln!(out, "{}", names[i])?; }
        for text in &results {
            let decoded = simdxml::XmlIndex::decode_entities(text);
            write!(out, "{decoded}{sep}")?;
        }
    }

    if json {
        let flat: Vec<String> = json_all.into_iter().flatten().collect();
        write_json(out, &flat)?;
    }
    out.flush()?;
    Ok(any)
}

// --- Sources ---

enum Source { File(String), Stdin }

impl Source {
    fn read_bytes(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        match self {
            Source::File(path) => Ok(fs::read(path)?),
            Source::Stdin => {
                let mut buf = Vec::new();
                io::stdin().read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }
    fn name(&self) -> &str {
        match self { Source::File(p) => p, Source::Stdin => "<stdin>" }
    }
}

fn gather_sources(files: &[&str]) -> Result<Vec<Source>, Box<dyn std::error::Error>> {
    if files.is_empty() {
        if io::stdin().is_terminal() {
            return Err("no input — provide files or pipe via stdin".into());
        }
        return Ok(vec![Source::Stdin]);
    }
    Ok(files.iter().map(|f| {
        if *f == "-" { Source::Stdin } else { Source::File(f.to_string()) }
    }).collect())
}

// --- JSON ---

fn write_json(out: &mut impl Write, items: &[String]) -> io::Result<()> {
    write!(out, "[")?;
    for (i, item) in items.iter().enumerate() {
        if i > 0 { write!(out, ",")?; }
        write!(out, "\"")?;
        for ch in item.chars() {
            match ch {
                '"' => write!(out, "\\\"")?,
                '\\' => write!(out, "\\\\")?,
                '\n' => write!(out, "\\n")?,
                '\r' => write!(out, "\\r")?,
                '\t' => write!(out, "\\t")?,
                c if c.is_control() => write!(out, "\\u{:04x}", c as u32)?,
                c => write!(out, "{c}")?,
            }
        }
        write!(out, "\"")?;
    }
    writeln!(out, "]")
}

// --- Info ---

fn run_info(args: &[String]) -> Result<bool, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("info: no files specified".into());
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    for (idx, file) in args.iter().enumerate() {
        let data = fs::read(file)?;
        let index = simdxml::parse(&data)?;

        if args.len() > 1 { writeln!(out, "{file}")?; }

        let size = data.len();
        writeln!(out, "  size: {}", humanize(size))?;
        writeln!(out, "  tags: {}", index.tag_count())?;
        writeln!(out, "  text ranges: {}", index.text_count())?;

        let mut name_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for i in 0..index.tag_count() {
            if index.tag_types[i] == simdxml::index::TagType::Open
                || index.tag_types[i] == simdxml::index::TagType::SelfClose
            {
                *name_counts.entry(index.tag_name(i)).or_default() += 1;
            }
        }
        let mut sorted: Vec<_> = name_counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));

        writeln!(out, "  unique tags: {}", sorted.len())?;
        let top = sorted.len().min(10);
        if top > 0 {
            writeln!(out, "  top tags:")?;
            for (name, cnt) in &sorted[..top] {
                writeln!(out, "    {cnt:>6}  {name}")?;
            }
            if sorted.len() > 10 {
                writeln!(out, "    ... and {} more", sorted.len() - 10)?;
            }
        }

        let max_depth = index.depths.iter().copied().max().unwrap_or(0);
        writeln!(out, "  max depth: {max_depth}")?;

        if args.len() > 1 && idx < args.len() - 1 { writeln!(out)?; }
    }
    out.flush()?;
    Ok(true)
}

fn humanize(bytes: usize) -> String {
    if bytes >= 1_073_741_824 { format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0) }
    else if bytes >= 1_048_576 { format!("{:.1} MiB", bytes as f64 / 1_048_576.0) }
    else if bytes >= 1024 { format!("{:.1} KiB", bytes as f64 / 1024.0) }
    else { format!("{bytes} B") }
}
