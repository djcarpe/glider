//! Deterministic LDBC-flavoured social graph generator, emitted as JSONL that
//! `glider <db> import` accepts directly.
//!
//! The shape is the part that matters. A uniform-random graph is the wrong
//! benchmark: real social graphs have a heavy-tailed degree distribution, so a
//! handful of nodes carry a disproportionate share of the edges, and that is
//! exactly what makes multi-hop traversal expensive. Edges here are drawn with
//! a copy model (preferential attachment), which reproduces that tail.
//!
//!   ldbc-gen --scale 100000 --out social.jsonl
//!
//! Schema, deliberately multi-label so the query mix has something to plan
//! against and the browser UI has something to draw:
//!
//!   (:Person {name, email, age, city, created})
//!   (:City   {name, country})
//!   (:Tag    {name})
//!   (:Post   {title, length, created})
//!
//!   (:Person)-[:KNOWS {since}]->(:Person)   heavy-tailed, the interesting one
//!   (:Person)-[:LIVES_IN]->(:City)
//!   (:Person)-[:AUTHORED]->(:Post)
//!   (:Post)-[:HAS_TAG]->(:Tag)

use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Write};

// --------------------------------------------------------------- prng

/// xorshift64*. Deterministic across platforms, no dependency, good enough for
/// generating a graph shape (this is not cryptography and not a statistics
/// paper).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Guard the zero state, which xorshift cannot leave.
        Rng(if seed == 0 { 0x2545_F491_4F6C_DD1D } else { seed })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, n). Modulo bias is irrelevant at these magnitudes.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    fn below_usize(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }
}

// --------------------------------------------------------------- vocab

const FIRST: &[&str] = &[
    "Ada", "Bob", "Cai", "Dara", "Eve", "Femi", "Gus", "Hana", "Ilya", "Jo", "Kit", "Lena", "Mo",
    "Nia", "Omar", "Pia", "Quinn", "Rafi", "Sun", "Tomas", "Uma", "Vik", "Wren", "Xu", "Yara",
    "Zane",
];

const LAST: &[&str] = &[
    "Ahmed", "Berg", "Chen", "Diaz", "Eriksen", "Fontaine", "Garcia", "Haddad", "Ivanov", "Jensen",
    "Kowalski", "Lindqvist", "Mensah", "Novak", "Okafor", "Petrov", "Rossi", "Silva", "Tanaka",
    "Ueda", "Varga", "Watanabe", "Yilmaz", "Zhang",
];

const CITIES: &[(&str, &str)] = &[
    ("London", "GB"), ("Paris", "FR"), ("Berlin", "DE"), ("Madrid", "ES"),
    ("Rome", "IT"), ("Lisbon", "PT"), ("Dublin", "IE"), ("Vienna", "AT"),
    ("Prague", "CZ"), ("Warsaw", "PL"), ("Athens", "GR"), ("Oslo", "NO"),
    ("Stockholm", "SE"), ("Helsinki", "FI"), ("Copenhagen", "DK"), ("Amsterdam", "NL"),
    ("Brussels", "BE"), ("Zurich", "CH"), ("Budapest", "HU"), ("Bucharest", "RO"),
    ("Tokyo", "JP"), ("Osaka", "JP"), ("Seoul", "KR"), ("Shanghai", "CN"),
    ("Mumbai", "IN"), ("Delhi", "IN"), ("Singapore", "SG"), ("Sydney", "AU"),
    ("Toronto", "CA"), ("Vancouver", "CA"), ("New York", "US"), ("Chicago", "US"),
    ("Austin", "US"), ("Seattle", "US"), ("Denver", "US"), ("Boston", "US"),
    ("Lagos", "NG"), ("Nairobi", "KE"), ("Cairo", "EG"), ("Cape Town", "ZA"),
    ("Sao Paulo", "BR"), ("Buenos Aires", "AR"), ("Santiago", "CL"), ("Bogota", "CO"),
    ("Mexico City", "MX"), ("Istanbul", "TR"), ("Tel Aviv", "IL"), ("Dubai", "AE"),
];

const TAGS: &[&str] = &[
    "graphs", "databases", "rust", "distributed-systems", "storage", "query-planning",
    "compilers", "networking", "security", "cryptography", "machine-learning", "statistics",
    "visualisation", "typography", "cycling", "climbing", "coffee", "baking", "jazz",
    "photography", "gardening", "chess", "hiking", "woodworking", "sailing", "astronomy",
    "linguistics", "history", "architecture", "ceramics",
];

const TITLE_HEAD: &[&str] = &[
    "Notes on", "Rethinking", "A short history of", "Against", "In praise of",
    "Benchmarking", "Debugging", "What I learned about", "The trouble with", "Revisiting",
];

// --------------------------------------------------------------- args

struct Args {
    scale: usize,
    seed: u64,
    out: Option<String>,
    avg_degree: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { scale: 100_000, seed: 42, out: None, avg_degree: 12 };
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let need = |i: usize| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", argv[i]))
        };
        match argv[i].as_str() {
            "--scale" | "-n" => {
                a.scale = parse_count(&need(i)?)?;
                i += 2;
            }
            "--seed" => {
                a.seed = need(i)?.parse().map_err(|_| "bad --seed".to_string())?;
                i += 2;
            }
            "--out" | "-o" => {
                a.out = Some(need(i)?);
                i += 2;
            }
            "--avg-degree" | "-d" => {
                a.avg_degree = need(i)?.parse().map_err(|_| "bad --avg-degree".to_string())?;
                i += 2;
            }
            "-h" | "--help" => {
                eprintln!(
                    "ldbc-gen --scale N [--avg-degree D] [--seed S] [--out FILE]\n\n\
                     N accepts k/m suffixes: --scale 500k, --scale 2m\n\
                     Writes JSONL on stdout when --out is omitted."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag {}", other)),
        }
    }
    if a.scale == 0 {
        return Err("--scale must be > 0".into());
    }
    if a.avg_degree == 0 {
        return Err("--avg-degree must be > 0".into());
    }
    Ok(a)
}

/// Accept 250000, 250k, 2m — the sizes get typed a lot on the command line.
fn parse_count(s: &str) -> Result<usize, String> {
    let s = s.trim().to_lowercase();
    let (digits, mult) = match s.strip_suffix('k') {
        Some(d) => (d, 1_000usize),
        None => match s.strip_suffix('m') {
            Some(d) => (d, 1_000_000usize),
            None => (s.as_str(), 1usize),
        },
    };
    digits
        .parse::<usize>()
        .map(|n| n * mult)
        .map_err(|_| format!("bad count {:?}", s))
}

// --------------------------------------------------------------- json

/// Escape per RFC 8259. The generated vocabulary is ASCII, but titles and names
/// are concatenated, so this stays honest rather than assuming.
fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// --------------------------------------------------------------- main

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ldbc-gen: {}", e);
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("ldbc-gen: {}", e);
        std::process::exit(1);
    }
}

fn run(args: &Args) -> io::Result<()> {
    let stdout = io::stdout();
    let mut w: BufWriter<Box<dyn Write>> = match &args.out {
        Some(p) => BufWriter::with_capacity(1 << 20, Box::new(File::create(p)?)),
        None => BufWriter::with_capacity(1 << 20, Box::new(stdout.lock())),
    };

    let persons = args.scale;
    let cities = CITIES.len();
    let tags = TAGS.len();
    let posts = persons / 2;

    // Ids are assigned by glider in insertion order starting at 0, and
    // import_jsonl maps a numeric "id" to the node it created. So emitting
    // nodes in a fixed order lets edges reference plain integers with no
    // string keys and no lookup table on the import side.
    //
    //   [0, persons)                      Person
    //   [persons, persons+cities)         City
    //   [+cities, +tags)                  Tag
    //   [+tags, +posts)                   Post
    let city_base = persons;
    let tag_base = city_base + cities;
    let post_base = tag_base + tags;

    let mut rng = Rng::new(args.seed);
    let mut line = String::with_capacity(256);

    // ---- nodes: Person
    for i in 0..persons {
        let first = FIRST[rng.below_usize(FIRST.len())];
        let last = LAST[rng.below_usize(LAST.len())];
        let city = CITIES[rng.below_usize(cities)].0;
        let age = 18 + rng.below(62); // 18..80
        let created = 1_262_304_000 + rng.below(505_000_000) as i64; // 2010..2026

        line.clear();
        line.push_str("{\"type\":\"node\",\"id\":");
        line.push_str(&i.to_string());
        line.push_str(",\"labels\":[\"Person\"],\"props\":{\"name\":");
        esc(&format!("{} {}", first, last), &mut line);
        line.push_str(",\"email\":");
        // Unique per node, so INDEX ON :Person(email) is a genuine point lookup
        // rather than a scan that happens to stop early.
        esc(&format!("{}.{}{}@example.com", first.to_lowercase(), last.to_lowercase(), i), &mut line);
        line.push_str(",\"age\":");
        line.push_str(&age.to_string());
        line.push_str(",\"city\":");
        esc(city, &mut line);
        line.push_str(",\"created\":");
        line.push_str(&created.to_string());
        line.push_str("}}\n");
        w.write_all(line.as_bytes())?;
    }

    // ---- nodes: City
    for (i, (name, country)) in CITIES.iter().enumerate() {
        line.clear();
        line.push_str("{\"type\":\"node\",\"id\":");
        line.push_str(&(city_base + i).to_string());
        line.push_str(",\"labels\":[\"City\"],\"props\":{\"name\":");
        esc(name, &mut line);
        line.push_str(",\"country\":");
        esc(country, &mut line);
        line.push_str("}}\n");
        w.write_all(line.as_bytes())?;
    }

    // ---- nodes: Tag
    for (i, t) in TAGS.iter().enumerate() {
        line.clear();
        line.push_str("{\"type\":\"node\",\"id\":");
        line.push_str(&(tag_base + i).to_string());
        line.push_str(",\"labels\":[\"Tag\"],\"props\":{\"name\":");
        esc(t, &mut line);
        line.push_str("}}\n");
        w.write_all(line.as_bytes())?;
    }

    // ---- nodes: Post
    for i in 0..posts {
        let head = TITLE_HEAD[rng.below_usize(TITLE_HEAD.len())];
        let tag = TAGS[rng.below_usize(tags)];
        let length = 200 + rng.below(4800);
        let created = 1_262_304_000 + rng.below(505_000_000) as i64;

        line.clear();
        line.push_str("{\"type\":\"node\",\"id\":");
        line.push_str(&(post_base + i).to_string());
        line.push_str(",\"labels\":[\"Post\"],\"props\":{\"title\":");
        esc(&format!("{} {}", head, tag), &mut line);
        line.push_str(",\"length\":");
        line.push_str(&length.to_string());
        line.push_str(",\"created\":");
        line.push_str(&created.to_string());
        line.push_str("}}\n");
        w.write_all(line.as_bytes())?;
    }

    // ---- edges: KNOWS, heavy-tailed via a copy model.
    //
    // `targets` holds every endpoint written so far; drawing from it uniformly
    // selects a node with probability proportional to its current degree, which
    // is what produces the power-law tail. A fraction of draws go to a uniformly
    // random node instead, which keeps the graph connected enough to be
    // traversable and stops the first few nodes from swallowing everything.
    let total_knows = persons.saturating_mul(args.avg_degree);
    let mut targets: Vec<u32> = Vec::with_capacity(total_knows.min(40_000_000) * 2);
    let mut knows = 0usize;

    for i in 0..persons {
        // Degree varies per node so the distribution is not a constant d.
        // Small integer draw, mean ~= avg_degree.
        let d = 1 + rng.below_usize(args.avg_degree * 2);
        for _ in 0..d {
            if knows >= total_knows {
                break;
            }
            let to = if targets.is_empty() || rng.below(100) < 25 {
                rng.below_usize(persons)
            } else {
                targets[rng.below_usize(targets.len())] as usize
            };
            if to == i {
                continue; // no self-loops
            }
            let since = 2010 + rng.below(16) as i64;

            line.clear();
            line.push_str("{\"type\":\"edge\",\"from\":");
            line.push_str(&i.to_string());
            line.push_str(",\"to\":");
            line.push_str(&to.to_string());
            line.push_str(",\"label\":\"KNOWS\",\"props\":{\"since\":");
            line.push_str(&since.to_string());
            line.push_str("}}\n");
            w.write_all(line.as_bytes())?;

            targets.push(i as u32);
            targets.push(to as u32);
            knows += 1;
        }
    }

    // ---- edges: LIVES_IN, one per person.
    for i in 0..persons {
        let c = city_base + rng.below_usize(cities);
        line.clear();
        line.push_str("{\"type\":\"edge\",\"from\":");
        line.push_str(&i.to_string());
        line.push_str(",\"to\":");
        line.push_str(&c.to_string());
        line.push_str(",\"label\":\"LIVES_IN\",\"props\":{}}\n");
        w.write_all(line.as_bytes())?;
    }

    // ---- edges: AUTHORED and HAS_TAG.
    for i in 0..posts {
        let author = rng.below_usize(persons);
        let post = post_base + i;
        line.clear();
        line.push_str("{\"type\":\"edge\",\"from\":");
        line.push_str(&author.to_string());
        line.push_str(",\"to\":");
        line.push_str(&post.to_string());
        line.push_str(",\"label\":\"AUTHORED\",\"props\":{}}\n");
        w.write_all(line.as_bytes())?;

        // One or two tags per post.
        let ntags = 1 + rng.below_usize(2);
        for _ in 0..ntags {
            let t = tag_base + rng.below_usize(tags);
            line.clear();
            line.push_str("{\"type\":\"edge\",\"from\":");
            line.push_str(&post.to_string());
            line.push_str(",\"to\":");
            line.push_str(&t.to_string());
            line.push_str(",\"label\":\"HAS_TAG\",\"props\":{}}\n");
            w.write_all(line.as_bytes())?;
        }
    }

    w.flush()?;

    let nodes = persons + cities + tags + posts;
    let edges = knows + persons + posts + posts; // approximate on tags (1..2)
    eprintln!(
        "ldbc-gen: {} nodes ({} Person, {} City, {} Tag, {} Post), ~{} edges ({} KNOWS), seed {}",
        nodes, persons, cities, tags, posts, edges, knows, args.seed
    );
    Ok(())
}
