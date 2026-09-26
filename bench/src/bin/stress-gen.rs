//! Writes a large, realistic glider database straight to a `.gldb` file, for
//! stress testing. Sized by bytes, not by node count:
//!
//!   stress-gen --size 1GiB --out bench/data/stress-1g.gldb
//!
//! Unlike `ldbc-gen`, which emits JSONL for `glider import`, this streams
//! records through `glider::store::LogWriter` and never builds the graph, so
//! memory stays flat however large the file. That matters because both
//! `import` and `open` hold the whole graph in RAM: the generator must not be
//! the thing that decides how big a test file can be.
//!
//! The data is the same small-town commerce world as scripts/mundane_graph.py,
//! grown in rounds until the file reaches the requested size, and made
//! deliberately awkward:
//!
//! - heavy-tailed degrees: KNOWS mixes a local community, preferential
//!   attachment towards early members, and uniform noise, so a few hubs carry
//!   tens of thousands of relationships — expand one of those in the browser
//! - skewed popularity: a few cities, employers and products dominate
//! - every value type: text, int (incl. negative and near-i64), float, bool,
//!   lists of text/int/float, explicit nulls, optional keys
//! - text of wildly varied length (a word to a few KB) and non-ASCII names,
//!   which takes the explorer's text search off its ASCII fast path
//! - multi-label nodes, and three property indexes to maintain
//! - history: each round also updates, unsets and relabels earlier nodes,
//!   deletes some relationships and some nodes, so replay exercises every
//!   kind of record, not just inserts
//!
//! Deterministic: the same --seed and --size give the same records (only the
//! header's random generation id differs).
//!
//! Schema
//!   (:Category {name, aisle})                   500
//!   (:City {name, country, population, lat, lon, coastal})  3,000
//!   per round (repeated until the file is big enough):
//!   (:Company {name, founded, employees, revenue, vat_registered, website, tags})       40
//!   (:Product {name, sku, price, in_stock, weight_kg, description, colour?, dims})      150
//!   (:Person[:Staff][:VIP] {name, email, born, phone, joined, balance, account_no,
//!            newsletter, hobbies?, address?, bio?, scores, notes?})                     1,000
//!   (:Order {ref, placed, status, payment, courier, items, total, gift})                500
//!   (:Review {stars, title, text, posted, verified, helpful})                           120
//!
//!   KNOWS, LIVES_IN, WORKS_AT, PLACED, CONTAINS, WROTE, ABOUT, SELLS, IN_CATEGORY,
//!   BASED_IN

use std::env;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use glider::store::{LogWriter, Op};
use glider::value::Value;

// ------------------------------------------------------------------- prng

/// xorshift64*: deterministic everywhere, no dependency. As in ldbc-gen.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(if seed == 0 {
            0x2545_F491_4F6C_DD1D
        } else {
            seed
        })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in [0, 1).
    fn f(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, p: f64) -> bool {
        self.f() < p
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
    /// Standard normal, Box–Muller.
    fn gauss(&mut self) -> f64 {
        let u = self.f().max(1e-12);
        let v = self.f();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
    /// An index in [0, n) biased towards 0: `skew` 1 is uniform, higher is
    /// steeper. This is where the hubs and best-sellers come from.
    fn skewed(&mut self, n: u64, skew: f64) -> u64 {
        ((n as f64) * self.f().powf(skew)) as u64 % n.max(1)
    }
    /// Heavy-tailed count >= 1: mostly small, occasionally large.
    fn heavy(&mut self, mean: f64, cap: u64) -> u64 {
        let x = (-self.f().max(1e-12).ln() * mean).powf(1.3);
        (1 + x as u64).min(cap)
    }
}

// ------------------------------------------------------------ vocabulary

const FIRST: &[&str] = &[
    "Aaron",
    "Abigail",
    "Adam",
    "Aisha",
    "Alice",
    "Amelia",
    "Amir",
    "Anna",
    "Arjun",
    "Arthur",
    "Ava",
    "Beatrice",
    "Ben",
    "Callum",
    "Charlotte",
    "Chloe",
    "Connor",
    "Daniel",
    "David",
    "Dylan",
    "Eleanor",
    "Ella",
    "Emily",
    "Ethan",
    "Fatima",
    "Fiona",
    "Freya",
    "George",
    "Grace",
    "Hannah",
    "Harry",
    "Hassan",
    "Henry",
    "Hugo",
    "Imogen",
    "Isaac",
    "Jack",
    "James",
    "Jasmine",
    "Joshua",
    "Julia",
    "Leah",
    "Leo",
    "Liam",
    "Lily",
    "Lucas",
    "Lucy",
    "Maya",
    "Mia",
    "Mohammed",
    "Noah",
    "Oliver",
    "Olivia",
    "Omar",
    "Oscar",
    "Poppy",
    "Priya",
    "Rachel",
    "Ruby",
    "Samuel",
    "Sarah",
    "Sofia",
    "Sophie",
    "Thomas",
    "William",
    "Yasmin",
    "Zachary",
    "Zara",
];
/// Non-ASCII names, drawn for about one person in six.
const FIRST_INTL: &[&str] = &[
    "José",
    "Zoë",
    "Øyvind",
    "Łukasz",
    "Siobhán",
    "Chloé",
    "Björn",
    "Ines",
    "Dvořák",
    "Mikaël",
    "Nguyễn",
    "Иван",
    "Ольга",
    "محمد",
    "فاطمة",
    "李",
    "王芳",
    "田中",
    "さくら",
    "민준",
    "Ağaç",
    "Søren",
    "Ángel",
    "François",
    "Jürgen",
    "Małgorzata",
];
const LAST: &[&str] = &[
    "Adams",
    "Ahmed",
    "Ali",
    "Allen",
    "Bailey",
    "Baker",
    "Begum",
    "Bell",
    "Brown",
    "Campbell",
    "Carter",
    "Clarke",
    "Collins",
    "Cooper",
    "Davies",
    "Edwards",
    "Evans",
    "Fisher",
    "Foster",
    "Green",
    "Hall",
    "Harris",
    "Hill",
    "Hughes",
    "Hussain",
    "Jackson",
    "Jones",
    "Kelly",
    "Khan",
    "King",
    "Lee",
    "Lewis",
    "Martin",
    "Mitchell",
    "Moore",
    "Morgan",
    "Murphy",
    "Patel",
    "Price",
    "Roberts",
    "Robinson",
    "Scott",
    "Shaw",
    "Singh",
    "Smith",
    "Taylor",
    "Thomas",
    "Turner",
    "Walker",
    "Ward",
    "White",
    "Williams",
    "Wilson",
    "Wood",
    "Wright",
    "Young",
    "Østergaard",
    "Müller",
    "García",
    "Nowak",
    "Kowalski",
    "Yılmaz",
    "Nakamura",
    "Kim",
    "Nguyen",
    "Da Silva",
];
const SYL_A: &[&str] = &[
    "Ash", "Bar", "Bel", "Brad", "Bur", "Car", "Chad", "Dun", "Eas", "Fair", "Glen", "Har", "Hol",
    "Kings", "Lang", "Mar", "Mel", "Nor", "Oak", "Pen", "Red", "Sal", "Stan", "Thorn", "Wal",
    "Wes", "Win", "Wood",
];
const SYL_B: &[&str] = &[
    "ford", "ton", "bury", "wick", "field", "ham", "mouth", "stead", "port", "bridge", "leigh",
    "worth", "dale", "by", "minster", "caster",
];
const COUNTRIES: &[&str] = &[
    "UK",
    "Ireland",
    "France",
    "Germany",
    "Netherlands",
    "Belgium",
    "Denmark",
    "Norway",
    "Sweden",
    "Spain",
    "Portugal",
    "Italy",
    "Poland",
    "Greece",
    "Türkiye",
    "Canada",
    "USA",
    "Japan",
    "South Korea",
    "Brasil",
    "Australia",
];
const ADJ: &[&str] = &[
    "Stainless",
    "Wooden",
    "Ceramic",
    "Cotton",
    "Linen",
    "Glass",
    "Bamboo",
    "Recycled",
    "Folding",
    "Insulated",
    "Non-stick",
    "Cordless",
    "Compact",
    "Large",
    "Small",
    "Waterproof",
    "Matte",
    "Glossy",
    "Striped",
    "Plain",
];
const NOUN: &[&str] = &[
    "Kettle",
    "Toaster",
    "Mug",
    "Teapot",
    "Saucepan",
    "Chopping Board",
    "Colander",
    "Whisk",
    "Bowl",
    "Plate",
    "Jug",
    "Tray",
    "Jar",
    "Flask",
    "Towel",
    "Bath Mat",
    "Bin",
    "Laundry Basket",
    "Duvet",
    "Pillow",
    "Blanket",
    "Cushion",
    "Rug",
    "Lamp",
    "Torch",
    "Notebook",
    "Pen",
    "Stapler",
    "Trowel",
    "Spade",
    "Hose",
    "Watering Can",
    "Umbrella",
    "Backpack",
    "Suitcase",
    "Scarf",
    "Slippers",
    "Boots",
    "Jumper",
    "Coat",
    "Tent",
    "Helmet",
    "Jigsaw",
    "Rolling Pin",
];
const COMPANY_SUFFIX: &[&str] = &[
    "Ltd",
    "& Sons",
    "Co.",
    "Supplies",
    "Trading",
    "Stores",
    "Group",
    "Direct",
    "Wholesale",
    "Homewares",
    "GmbH",
    "S.A.",
    "株式会社",
];
const STATUS: &[&str] = &[
    "delivered",
    "delivered",
    "delivered",
    "delivered",
    "delivered",
    "delivered",
    "shipped",
    "shipped",
    "processing",
    "cancelled",
    "returned",
];
const PAYMENT: &[&str] = &[
    "card",
    "card",
    "card",
    "paypal",
    "bank transfer",
    "gift card",
];
const COURIER: &[&str] = &["Royal Mail", "DPD", "Evri", "UPS", "DHL", "Yodel"];
const STREETS: &[&str] = &[
    "High Street",
    "Church Lane",
    "Station Road",
    "Mill Lane",
    "Park Avenue",
    "Victoria Road",
    "The Green",
    "King's Road",
    "Queen Street",
    "Bridge Street",
    "Market Square",
    "Rue de la Paix",
    "Hauptstraße",
    "Calle Mayor",
];
const DEPTS: &[&str] = &[
    "Sales",
    "Support",
    "Warehouse",
    "Accounts",
    "Marketing",
    "Engineering",
    "Dispatch",
    "Buying",
];
const JOBS: &[&str] = &[
    "Assistant",
    "Manager",
    "Supervisor",
    "Clerk",
    "Analyst",
    "Technician",
    "Apprentice",
    "Lead",
];
const HOBBIES: &[&str] = &[
    "gardening",
    "cycling",
    "baking",
    "hiking",
    "knitting",
    "chess",
    "running",
    "fishing",
    "photography",
    "reading",
    "painting",
    "swimming",
    "birdwatching",
    "pottery",
    "football",
    "board-games",
    "camping",
    "DIY",
];
const TAGS: &[&str] = &[
    "family-run",
    "b-corp",
    "eco",
    "since-1900s",
    "online-only",
    "franchise",
    "co-op",
    "import",
    "export",
    "award-winning",
];
const HOW: &[&str] = &["school", "work", "neighbours", "family", "club", "online"];
const COLOURS: &[&str] = &[
    "black", "white", "grey", "navy", "green", "red", "natural", "cream",
];
const HOW_REL: &[&str] = &["friend", "colleague", "relative", "acquaintance"];
/// Sentences for reviews, bios and descriptions; strung together to lengths
/// from a few words to a few kilobytes.
const SENTENCES: &[&str] = &[
    "Does exactly what it says.",
    "Arrived quickly, well packaged.",
    "Good value for the price.",
    "Sturdier than I expected.",
    "Second one I've bought — the first is still going strong.",
    "Fits neatly in the cupboard.",
    "Nice colour, matches the kitchen.",
    "Slightly smaller than the picture suggests.",
    "The instructions were unclear, but it works.",
    "Took a while to arrive.",
    "Broke after two weeks.",
    "Not as described.",
    "Handle came loose almost immediately.",
    "Smelled of plastic for days.",
    "Très bien, je recommande.",
    "Sehr gute Qualität, schnelle Lieferung.",
    "Muy práctico para el día a día.",
    "値段の割に良い。",
    "Would buy again 👍",
    "Customer service sorted it out within a day.",
    "Lives on the windowsill now and gets compliments.",
    "Enjoys long walks, allotment gardening and the pub quiz on Thursdays.",
    "Volunteers at the library on Saturdays.",
    "Keen amateur photographer; mostly birds, occasionally trains.",
];

// ---------------------------------------------------------------- layout
//
// Ids are assigned here, not by the engine, so any node can be named by
// arithmetic instead of a lookup table: memory stays flat.

const CATEGORIES: u64 = 500;
const CITIES: u64 = 3_000;
const PREFIX: u64 = CATEGORIES + CITIES;

const R_COMPANY: u64 = 40;
const R_PRODUCT: u64 = 150;
const R_PERSON: u64 = 1_000;
const R_ORDER: u64 = 500;
const R_REVIEW: u64 = 120;
const ROUND: u64 = R_COMPANY + R_PRODUCT + R_PERSON + R_ORDER + R_REVIEW;

const OFF_COMPANY: u64 = 0;
const OFF_PRODUCT: u64 = OFF_COMPANY + R_COMPANY;
const OFF_PERSON: u64 = OFF_PRODUCT + R_PRODUCT;
const OFF_ORDER: u64 = OFF_PERSON + R_PERSON;
const OFF_REVIEW: u64 = OFF_ORDER + R_ORDER;

fn category_id(i: u64) -> u64 {
    1 + i
}
fn city_id(i: u64) -> u64 {
    1 + CATEGORIES + i
}
/// The id of the `i`th entity of a per-round kind, across all rounds.
fn nth(i: u64, per_round: u64, offset: u64) -> u64 {
    1 + PREFIX + (i / per_round) * ROUND + offset + i % per_round
}
fn company_id(i: u64) -> u64 {
    nth(i, R_COMPANY, OFF_COMPANY)
}
fn product_id(i: u64) -> u64 {
    nth(i, R_PRODUCT, OFF_PRODUCT)
}
fn person_id(i: u64) -> u64 {
    nth(i, R_PERSON, OFF_PERSON)
}
fn order_id(i: u64) -> u64 {
    nth(i, R_ORDER, OFF_ORDER)
}
fn review_id(i: u64) -> u64 {
    nth(i, R_REVIEW, OFF_REVIEW)
}

// -------------------------------------------------------------- helpers

fn t(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

fn date(rng: &mut Rng, y0: i64, y1: i64) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        rng.range(y0, y1),
        rng.range(1, 12),
        rng.range(1, 28)
    )
}

/// Prose of heavy-tailed length: usually a sentence or two, sometimes pages.
fn prose(rng: &mut Rng, mean_sentences: f64, cap: u64) -> String {
    let n = rng.heavy(mean_sentences, cap);
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(rng.pick(SENTENCES));
    }
    s
}

fn person_name(rng: &mut Rng) -> (String, String) {
    let first = if rng.chance(0.16) {
        rng.pick(FIRST_INTL)
    } else {
        rng.pick(FIRST)
    };
    let last = rng.pick(LAST);
    (first.to_string(), last.to_string())
}

/// ASCII-ish local part for an email, whatever the name's script.
fn slug(s: &str) -> String {
    let out: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if out.is_empty() {
        "user".into()
    } else {
        out
    }
}

struct Gen<'w> {
    w: &'w mut LogWriter,
    rng: Rng,
    next_edge: u64,
    nodes: u64,
    edges: u64,
    deleted_nodes: u64,
    deleted_edges: u64,
    updates: u64,
}

impl Gen<'_> {
    fn node(&mut self, id: u64, labels: &[&str], props: Vec<(&str, Value)>) -> io::Result<()> {
        self.nodes += 1;
        self.w.push(&Op::NodeAdd {
            id,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            props: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        })
    }

    fn edge(
        &mut self,
        from: u64,
        to: u64,
        etype: &str,
        props: Vec<(&str, Value)>,
    ) -> io::Result<u64> {
        let id = self.next_edge;
        self.next_edge += 1;
        self.edges += 1;
        self.w.push(&Op::EdgeAdd {
            id,
            from,
            to,
            etype: etype.to_string(),
            props: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        })?;
        Ok(id)
    }

    fn prefix(&mut self) -> io::Result<()> {
        for (label, key) in [("Person", "email"), ("Order", "ref"), ("Product", "sku")] {
            self.w.push(&Op::IndexAdd {
                label: label.into(),
                key: key.into(),
            })?;
        }
        for i in 0..CATEGORIES {
            let name = format!("{} {}", self.rng.pick(ADJ), self.rng.pick(NOUN));
            let aisle = self.rng.range(1, 40);
            self.node(
                category_id(i),
                &["Category"],
                vec![("name", t(name)), ("aisle", Value::Int(aisle))],
            )?;
        }
        for i in 0..CITIES {
            let name = format!("{}{}", self.rng.pick(SYL_A), self.rng.pick(SYL_B));
            let name = if i >= 400 {
                format!("{} {}", name, i)
            } else {
                name
            };
            let props = vec![
                ("name", t(name)),
                ("country", t(self.rng.pick(COUNTRIES))),
                (
                    "population",
                    Value::Int((self.rng.gauss() * 1.2 + 10.0).exp() as i64),
                ),
                ("lat", Value::Float(self.rng.f() * 120.0 - 50.0)),
                ("lon", Value::Float(self.rng.f() * 360.0 - 180.0)),
                ("coastal", Value::Bool(self.rng.chance(0.3))),
            ];
            self.node(city_id(i), &["City"], props)?;
        }
        self.w.commit()
    }

    /// One round: its nodes, then the relationships from and to them, then
    /// some history against what already exists.
    fn round(&mut self, r: u64) -> io::Result<()> {
        // ---- companies
        for j in 0..R_COMPANY {
            let i = r * R_COMPANY + j;
            let surname = self.rng.pick(LAST);
            let name = match self.rng.below(3) {
                0 => format!("{} {}", surname, self.rng.pick(COMPANY_SUFFIX)),
                1 => format!("{} & {}", surname, self.rng.pick(LAST)),
                _ => format!(
                    "{}{} {}",
                    self.rng.pick(SYL_A),
                    self.rng.pick(SYL_B),
                    self.rng.pick(COMPANY_SUFFIX)
                ),
            };
            let tags: Vec<Value> = (0..self.rng.below(4))
                .map(|_| t(self.rng.pick(TAGS)))
                .collect();
            let props = vec![
                ("name", t(name)),
                ("founded", Value::Int(self.rng.range(1850, 2026))),
                (
                    "employees",
                    Value::Int((self.rng.gauss() * 1.4 + 3.0).exp() as i64 + 1),
                ),
                (
                    "revenue",
                    Value::Float((self.rng.gauss() * 2.0 + 13.0).exp()),
                ),
                ("vat_registered", Value::Bool(self.rng.chance(0.8))),
                ("website", t(format!("www.{}{}.example", slug(surname), i))),
                ("tags", Value::List(tags)),
            ];
            self.node(company_id(i), &["Company"], props)?;
        }

        // ---- products
        for j in 0..R_PRODUCT {
            let i = r * R_PRODUCT + j;
            let mut props = vec![
                (
                    "name",
                    t(format!("{} {}", self.rng.pick(ADJ), self.rng.pick(NOUN))),
                ),
                ("sku", t(format!("SKU-{:09}", i))),
                (
                    "price",
                    Value::Float((self.rng.f() * 150.0 * 100.0).round() / 100.0 + 0.99),
                ),
                ("in_stock", Value::Bool(self.rng.chance(0.85))),
                (
                    "weight_kg",
                    Value::Float((self.rng.f() * 1200.0).round() / 100.0),
                ),
                ("description", t(prose(&mut self.rng, 2.0, 12))),
                (
                    "dims",
                    Value::List(
                        (0..3)
                            .map(|_| Value::Float((self.rng.f() * 800.0).round() / 10.0))
                            .collect(),
                    ),
                ),
            ];
            if self.rng.chance(0.3) {
                props.push(("colour", t(self.rng.pick(COLOURS))));
            }
            self.node(product_id(i), &["Product"], props)?;
        }

        // ---- people
        for j in 0..R_PERSON {
            let i = r * R_PERSON + j;
            let (first, last) = person_name(&mut self.rng);
            let born = self.rng.range(1935, 2008);
            let mut props = vec![
                ("name", t(format!("{} {}", first, last))),
                (
                    "email",
                    t(format!("{}.{}{}@example.com", slug(&first), slug(&last), i)),
                ),
                ("born", Value::Int(born)),
                (
                    "phone",
                    t(format!(
                        "+44 7{:03} {:06}",
                        self.rng.below(1000),
                        self.rng.below(1_000_000)
                    )),
                ),
                ("joined", t(date(&mut self.rng, 2010, 2026))),
                (
                    "balance",
                    Value::Float(((self.rng.gauss() * 400.0) * 100.0).round() / 100.0),
                ),
                (
                    "account_no",
                    Value::Int(i64::MAX - self.rng.below(1 << 40) as i64),
                ),
                ("newsletter", Value::Bool(self.rng.chance(0.4))),
                (
                    "scores",
                    Value::List(
                        (0..self.rng.below(6))
                            .map(|_| Value::Int(self.rng.range(-5, 100)))
                            .collect(),
                    ),
                ),
            ];
            if self.rng.chance(0.5) {
                let n = self.rng.range(1, 4);
                props.push((
                    "hobbies",
                    Value::List((0..n).map(|_| t(self.rng.pick(HOBBIES))).collect()),
                ));
            }
            if self.rng.chance(0.7) {
                props.push((
                    "address",
                    t(format!(
                        "{} {}",
                        self.rng.range(1, 250),
                        self.rng.pick(STREETS)
                    )),
                ));
            }
            if self.rng.chance(0.2) {
                props.push(("bio", t(prose(&mut self.rng, 1.5, 30))));
            }
            if self.rng.chance(0.03) {
                props.push(("notes", Value::Null));
            }
            let labels: &[&str] = if i % 40 == 0 {
                &["Person", "Staff"]
            } else {
                &["Person"]
            };
            self.node(person_id(i), labels, props)?;
        }

        // ---- orders
        for j in 0..R_ORDER {
            let i = r * R_ORDER + j;
            let props = vec![
                ("ref", t(format!("ORD-{}-{:09}", 2019 + i % 8, i))),
                ("placed", t(date(&mut self.rng, 2019, 2026))),
                ("status", t(self.rng.pick(STATUS))),
                ("payment", t(self.rng.pick(PAYMENT))),
                ("courier", t(self.rng.pick(COURIER))),
                ("items", Value::Int(0)),
                ("total", Value::Float(0.0)),
                ("gift", Value::Bool(self.rng.chance(0.08))),
            ];
            self.node(order_id(i), &["Order"], props)?;
        }

        // ---- reviews
        for j in 0..R_REVIEW {
            let i = r * R_REVIEW + j;
            let stars = [1, 2, 3, 4, 4, 5, 5, 5][self.rng.below(8) as usize];
            let props = vec![
                ("stars", Value::Int(stars)),
                ("title", t(self.rng.pick(SENTENCES))),
                ("text", t(prose(&mut self.rng, 3.0, 80))),
                ("posted", t(date(&mut self.rng, 2020, 2026))),
                ("verified", Value::Bool(self.rng.chance(0.7))),
                ("helpful", Value::Int(self.rng.heavy(3.0, 5000) as i64 - 1)),
            ];
            self.node(review_id(i), &["Review"], props)?;
        }

        // Everything up to the end of this round exists now; relationships
        // may point anywhere in it.
        let companies = (r + 1) * R_COMPANY;
        let products = (r + 1) * R_PRODUCT;
        let people = (r + 1) * R_PERSON;

        for j in 0..R_COMPANY {
            let i = r * R_COMPANY + j;
            let city = city_id(self.rng.skewed(CITIES, 2.5));
            let since = self.rng.range(1990, 2026);
            self.edge(
                company_id(i),
                city,
                "BASED_IN",
                vec![("since", Value::Int(since))],
            )?;
        }

        for j in 0..R_PRODUCT {
            let i = r * R_PRODUCT + j;
            let cat = category_id(self.rng.below(CATEGORIES));
            self.edge(product_id(i), cat, "IN_CATEGORY", vec![])?;
            if self.rng.chance(0.15) {
                let cat = category_id(self.rng.below(CATEGORIES));
                self.edge(
                    product_id(i),
                    cat,
                    "IN_CATEGORY",
                    vec![("secondary", Value::Bool(true))],
                )?;
            }
            for _ in 0..self.rng.range(1, 3) {
                let co = company_id(self.rng.skewed(companies, 1.8));
                let margin = (self.rng.f() * 55.0).round() / 100.0 + 0.05;
                self.edge(
                    co,
                    product_id(i),
                    "SELLS",
                    vec![("margin", Value::Float(margin))],
                )?;
            }
        }

        let mut knows_this_round = Vec::new();
        for j in 0..R_PERSON {
            let i = r * R_PERSON + j;
            let me = person_id(i);
            let city = city_id(self.rng.skewed(CITIES, 2.5));
            let since = self.rng.range(1990, 2026);
            self.edge(me, city, "LIVES_IN", vec![("since", Value::Int(since))])?;
            if self.rng.chance(0.85) {
                let co = company_id(self.rng.skewed(companies, 2.2));
                let role = format!("{} {}", self.rng.pick(DEPTS), self.rng.pick(JOBS));
                let props = vec![
                    ("role", t(role)),
                    ("since", Value::Int(self.rng.range(1995, 2026))),
                    ("part_time", Value::Bool(self.rng.chance(0.25))),
                    ("salary", Value::Int(self.rng.range(18_000, 140_000))),
                ];
                self.edge(me, co, "WORKS_AT", props)?;
            }
            // Friends: mostly a local community, some drawn towards the early
            // members (the hubs), a little uniform noise. Heavy-tailed count.
            for _ in 0..self.rng.heavy(3.0, 400) {
                let x = self.rng.f();
                let other = if x < 0.6 {
                    let d = (self.rng.gauss() * 600.0) as i64;
                    (i as i64 + d).clamp(0, people as i64 - 1) as u64
                } else if x < 0.9 {
                    self.rng.skewed(people, 3.0)
                } else {
                    self.rng.below(people)
                };
                if other == i {
                    continue;
                }
                let props = vec![
                    ("since", Value::Int(self.rng.range(1995, 2026))),
                    ("how", t(self.rng.pick(HOW))),
                    ("kind", t(self.rng.pick(HOW_REL))),
                    (
                        "weight",
                        Value::Float((self.rng.f() * 100.0).round() / 100.0),
                    ),
                ];
                let e = self.edge(me, person_id(other), "KNOWS", props)?;
                knows_this_round.push(e);
            }
        }

        for j in 0..R_ORDER {
            let i = r * R_ORDER + j;
            let buyer = person_id(self.rng.skewed(people, 1.6));
            let placed = date(&mut self.rng, 2019, 2026);
            self.edge(buyer, order_id(i), "PLACED", vec![("on", t(placed))])?;
            let n = self.rng.heavy(1.5, 40);
            let mut total = 0.0;
            for _ in 0..n {
                let p = self.rng.skewed(products, 2.0);
                let qty = [1, 1, 1, 2, 2, 3, 6][self.rng.below(7) as usize];
                let unit = (self.rng.f() * 150.0 * 100.0).round() / 100.0 + 0.99;
                total += unit * qty as f64;
                let props = vec![("qty", Value::Int(qty)), ("unit_price", Value::Float(unit))];
                self.edge(order_id(i), product_id(p), "CONTAINS", props)?;
            }
            // The totals are only known now: history, in the order a real
            // system would have written it.
            self.update(order_id(i), "items", Value::Int(n as i64))?;
            self.update(
                order_id(i),
                "total",
                Value::Float((total * 100.0).round() / 100.0),
            )?;
        }

        for j in 0..R_REVIEW {
            let i = r * R_REVIEW + j;
            let author = person_id(self.rng.skewed(people, 1.5));
            self.edge(author, review_id(i), "WROTE", vec![])?;
            let p = product_id(self.rng.skewed(products, 2.0));
            self.edge(review_id(i), p, "ABOUT", vec![])?;
        }

        // ---- history against earlier rounds
        for _ in 0..R_PERSON / 25 {
            let p = person_id(self.rng.below(people));
            let seen = date(&mut self.rng, 2024, 2026);
            self.update(p, "last_seen", t(seen))?;
        }
        for _ in 0..R_PERSON / 200 {
            let p = person_id(self.rng.below(people));
            self.updates += 1;
            self.w.push(&Op::NodeUnset {
                id: p,
                key: "phone".into(),
            })?;
        }
        for _ in 0..R_PERSON / 150 {
            let p = person_id(self.rng.skewed(people, 1.5));
            self.updates += 1;
            self.w.push(&Op::LabelAdd {
                id: p,
                label: "VIP".into(),
            })?;
        }
        for _ in 0..R_ORDER / 20 {
            let o = order_id(self.rng.below((r + 1) * R_ORDER));
            self.update(o, "status", t("returned"))?;
        }
        // Some friendships end.
        for (k, e) in knows_this_round.iter().enumerate() {
            if k % 97 == 13 {
                self.deleted_edges += 1;
                self.w.push(&Op::EdgeDel { id: *e })?;
            }
        }
        // Some reviews are withdrawn; nothing later points at a review, so
        // deleting one (and, detached, its WROTE and ABOUT) is always safe.
        for j in (0..R_REVIEW).step_by(53) {
            self.deleted_nodes += 1;
            self.w.push(&Op::NodeDel {
                id: review_id(r * R_REVIEW + j),
            })?;
        }

        self.w.commit()
    }

    fn update(&mut self, id: u64, key: &str, value: Value) -> io::Result<()> {
        self.updates += 1;
        self.w.push(&Op::NodeSet {
            id,
            key: key.into(),
            value,
        })
    }
}

// ------------------------------------------------------------------ cli

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("bad size '{}'", s))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        u => return Err(format!("unknown unit '{}' (use KiB, MiB, GiB)", u)),
    };
    Ok((n * mult as f64) as u64)
}

const USAGE: &str =
    "usage: stress-gen --size <500MiB|1GiB|...> --out <file.gldb> [--seed N] [--force]";

fn main() {
    if let Err(e) = run() {
        eprintln!("stress-gen: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut size = None;
    let mut out = None;
    let mut seed = 20_260_926u64;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        let val = |i: usize| {
            args.get(i + 1)
                .cloned()
                .ok_or(format!("{} needs a value\n{}", args[i], USAGE))
        };
        match args[i].as_str() {
            "--size" => {
                size = Some(parse_size(&val(i)?)?);
                i += 1;
            }
            "--out" => {
                out = Some(PathBuf::from(val(i)?));
                i += 1;
            }
            "--seed" => {
                seed = val(i)?.parse().map_err(|_| "bad --seed")?;
                i += 1;
            }
            "--force" => force = true,
            "-h" | "--help" => {
                println!("{}", USAGE);
                return Ok(());
            }
            a => return Err(format!("unexpected argument '{}'\n{}", a, USAGE)),
        }
        i += 1;
    }
    let size = size.ok_or(USAGE)?;
    let out = out.ok_or(USAGE)?;
    if force {
        let _ = std::fs::remove_file(&out);
    }

    let mut w = LogWriter::create(&out).map_err(|e| format!("{}: {}", out.display(), e))?;
    let mut g = Gen {
        w: &mut w,
        rng: Rng::new(seed),
        next_edge: 1,
        nodes: 0,
        edges: 0,
        deleted_nodes: 0,
        deleted_edges: 0,
        updates: 0,
    };
    let start = Instant::now();
    let mut last = Instant::now();
    g.prefix().map_err(|e| e.to_string())?;
    let mut r = 0;
    while g.w.bytes() < size {
        g.round(r).map_err(|e| e.to_string())?;
        r += 1;
        if last.elapsed().as_secs_f64() >= 2.0 {
            last = Instant::now();
            let b = g.w.bytes();
            eprintln!(
                "  {:>6.1}% {:>8.1} MiB  {:>11} nodes  {:>12} rels  {:>6.0} MiB/s",
                100.0 * b as f64 / size as f64,
                b as f64 / (1 << 20) as f64,
                g.nodes,
                g.edges,
                b as f64 / (1 << 20) as f64 / start.elapsed().as_secs_f64(),
            );
        }
    }
    let (nodes, edges, dn, de, up) = (
        g.nodes,
        g.edges,
        g.deleted_nodes,
        g.deleted_edges,
        g.updates,
    );
    let ops = w.ops();
    let bytes = w.finish().map_err(|e| e.to_string())?;
    println!(
        "{}: {:.2} GiB, {} rounds, {} nodes ({} live), {} relationships written ({} deleted, plus those of deleted nodes), {} updates, {} records, {:.1}s",
        out.display(),
        bytes as f64 / (1u64 << 30) as f64,
        r,
        nodes,
        nodes - dn,
        edges,
        de,
        up,
        ops,
        start.elapsed().as_secs_f64(),
    );
    Ok(())
}
