//! Calculator: an expression parser and a table of units. No `eval`, no
//! network: currencies are refused.

use crate::ui::tr;

#[derive(Debug, Clone, PartialEq)]
pub enum CalcError {
    Empty,
    Syntax,
    DivZero,
    Range,
    UnknownUnit(String),
    Incompatible,
    Currency,
}

impl CalcError {
    pub fn message(&self) -> String {
        match self {
            CalcError::Empty => tr("Tape un calcul ou une conversion", "Type a sum or a conversion").to_string(),
            CalcError::Syntax => tr("Expression invalide", "Invalid expression").to_string(),
            CalcError::DivZero => tr("Division par zéro", "Division by zero").to_string(),
            CalcError::Range => tr("Résultat hors limites", "Result out of range").to_string(),
            CalcError::UnknownUnit(u) => format!("{} {u}", tr("Unité inconnue :", "Unknown unit:")),
            CalcError::Incompatible => tr("Unités incompatibles", "Incompatible units").to_string(),
            CalcError::Currency => tr("Devises indisponibles hors ligne", "Currencies need a network").to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    LParen,
    RParen,
    Percent,
}

fn tokenize(src: &str) -> Result<Vec<Tok>, CalcError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let simple = match c {
            '+' => Some(Tok::Plus),
            '-' | '−' | '–' => Some(Tok::Minus),
            '*' | '×' | '·' => Some(Tok::Star),
            '/' | '÷' => Some(Tok::Slash),
            '^' => Some(Tok::Caret),
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            '%' => Some(Tok::Percent),
            _ => None,
        };
        if let Some(tok) = simple {
            out.push(tok);
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' || c == ',' {
            let start = i;
            let mut digits = 0;
            let mut separators = 0;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == ',') {
                if chars[i].is_ascii_digit() {
                    digits += 1;
                } else {
                    separators += 1;
                }
                i += 1;
            }
            if digits == 0 || separators > 1 {
                return Err(CalcError::Syntax);
            }
            let text: String = chars[start..i].iter().map(|&c| if c == ',' { '.' } else { c }).collect();
            out.push(Tok::Num(text.parse().map_err(|_| CalcError::Syntax)?));
        } else {
            return Err(CalcError::Syntax);
        }
    }
    Ok(out)
}

#[derive(Debug)]
enum Node {
    Num(f64),
    Neg(Box<Node>),
    Pct(Box<Node>),
    Bin(Op, Box<Node>, Box<Node>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

const MAX_DEPTH: u32 = 48;

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.toks.get(self.at + 1)
    }

    fn expr(&mut self) -> Result<Node, CalcError> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => Op::Add,
                Some(Tok::Minus) => Op::Sub,
                _ => break,
            };
            self.at += 1;
            left = Node::Bin(op, Box::new(left), Box::new(self.term()?));
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Node, CalcError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => Op::Mul,
                Some(Tok::Slash) => Op::Div,
                // `%` followed by an operand is a remainder, otherwise a percentage.
                Some(Tok::Percent) if matches!(self.peek2(), Some(Tok::Num(_) | Tok::LParen)) => Op::Mod,
                _ => break,
            };
            self.at += 1;
            left = Node::Bin(op, Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Node, CalcError> {
        match self.peek() {
            Some(Tok::Minus) => {
                self.at += 1;
                self.nested(|p| p.unary().map(|n| Node::Neg(Box::new(n))))
            }
            Some(Tok::Plus) => {
                self.at += 1;
                self.nested(Parser::unary)
            }
            _ => self.power(),
        }
    }

    fn power(&mut self) -> Result<Node, CalcError> {
        let base = self.postfix()?;
        if self.peek() == Some(&Tok::Caret) {
            self.at += 1;
            let exp = self.nested(Parser::unary)?;
            return Ok(Node::Bin(Op::Pow, Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    fn postfix(&mut self) -> Result<Node, CalcError> {
        let mut node = self.primary()?;
        while self.peek() == Some(&Tok::Percent) && !matches!(self.peek2(), Some(Tok::Num(_) | Tok::LParen)) {
            self.at += 1;
            node = Node::Pct(Box::new(node));
        }
        Ok(node)
    }

    fn primary(&mut self) -> Result<Node, CalcError> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.at += 1;
                Ok(Node::Num(n))
            }
            Some(Tok::LParen) => {
                self.at += 1;
                let inner = self.nested(Parser::expr)?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(CalcError::Syntax);
                }
                self.at += 1;
                Ok(inner)
            }
            _ => Err(CalcError::Syntax),
        }
    }

    /// Bounds the recursion so a long run of brackets cannot overflow the stack.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Parser) -> Result<T, CalcError>) -> Result<T, CalcError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(CalcError::Syntax);
        }
        let out = f(self);
        self.depth -= 1;
        out
    }
}

fn finite(v: f64) -> Result<f64, CalcError> {
    if v.is_finite() { Ok(v) } else { Err(CalcError::Range) }
}

fn eval_node(n: &Node) -> Result<f64, CalcError> {
    match n {
        Node::Num(v) => Ok(*v),
        Node::Neg(a) => Ok(-eval_node(a)?),
        Node::Pct(a) => Ok(eval_node(a)? / 100.0),
        Node::Bin(op, a, b) => {
            let left = eval_node(a)?;
            // "200 + 10%" is 200 plus ten percent of 200.
            let right = match (op, &**b) {
                (Op::Add | Op::Sub, Node::Pct(inner)) => left * eval_node(inner)? / 100.0,
                _ => eval_node(b)?,
            };
            let v = match op {
                Op::Add => left + right,
                Op::Sub => left - right,
                Op::Mul => left * right,
                Op::Div | Op::Mod if right == 0.0 => return Err(CalcError::DivZero),
                Op::Div => left / right,
                Op::Mod => left % right,
                Op::Pow => left.powf(right),
            };
            finite(v)
        }
    }
}

/// Evaluates an arithmetic expression.
pub fn eval(src: &str) -> Result<f64, CalcError> {
    let toks = tokenize(src)?;
    if toks.is_empty() {
        return Err(CalcError::Empty);
    }
    let mut p = Parser { toks, at: 0, depth: 0 };
    let node = p.expr()?;
    if p.at != p.toks.len() {
        return Err(CalcError::Syntax);
    }
    finite(eval_node(&node)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dim {
    Length,
    Mass,
    Time,
    Data,
    Area,
    Volume,
    Speed,
    Pressure,
    Energy,
    Power,
    Angle,
    Temp,
}

/// `base = value * factor + offset`.
struct Unit {
    names: &'static [&'static str],
    dim: Dim,
    factor: f64,
    offset: f64,
}

const fn u(names: &'static [&'static str], dim: Dim, factor: f64) -> Unit {
    Unit { names, dim, factor, offset: 0.0 }
}

const KELVIN: f64 = 273.15;

/// Earlier rows win when a name matches case-insensitively only.
static UNITS: &[Unit] = &[
    u(&["m", "mètre", "metre", "mètres", "metres", "meter", "meters"], Dim::Length, 1.0),
    u(&["km", "kilomètre", "kilomètres", "kilometre", "kilometres", "kilometer", "kilometers"], Dim::Length, 1000.0),
    u(&["cm", "centimètre", "centimètres", "centimeter", "centimeters"], Dim::Length, 0.01),
    u(&["mm", "millimètre", "millimètres", "millimeter", "millimeters"], Dim::Length, 0.001),
    u(&["µm", "um", "micron", "microns"], Dim::Length, 1e-6),
    u(&["mi", "mile", "miles"], Dim::Length, 1609.344),
    u(&["yd", "yard", "yards"], Dim::Length, 0.9144),
    u(&["ft", "pied", "pieds", "foot", "feet"], Dim::Length, 0.3048),
    u(&["in", "\"", "pouce", "pouces", "inch", "inches"], Dim::Length, 0.0254),
    u(&["nmi", "NM", "mille marin", "milles marins"], Dim::Length, 1852.0),
    u(&["kg", "kilo", "kilos", "kilogramme", "kilogrammes"], Dim::Mass, 1.0),
    u(&["g", "gramme", "grammes", "gram", "grams"], Dim::Mass, 0.001),
    u(&["mg", "milligramme", "milligrammes"], Dim::Mass, 1e-6),
    u(&["t", "tonne", "tonnes"], Dim::Mass, 1000.0),
    u(&["lb", "lbs", "livre", "livres", "pound", "pounds"], Dim::Mass, 0.45359237),
    u(&["oz", "once", "onces", "ounce", "ounces"], Dim::Mass, 0.028349523125),
    u(&["s", "sec", "seconde", "secondes", "second", "seconds"], Dim::Time, 1.0),
    u(&["ms", "milliseconde", "millisecondes", "millisecond", "milliseconds"], Dim::Time, 0.001),
    u(&["min", "mn", "minute", "minutes"], Dim::Time, 60.0),
    u(&["h", "hr", "heure", "heures", "hour", "hours"], Dim::Time, 3600.0),
    u(&["j", "d", "jour", "jours", "day", "days"], Dim::Time, 86_400.0),
    u(&["sem", "semaine", "semaines", "wk", "week", "weeks"], Dim::Time, 604_800.0),
    u(&["mois", "month", "months"], Dim::Time, 2_629_746.0),
    u(&["an", "ans", "yr", "year", "years"], Dim::Time, 31_556_952.0),
    u(&["o", "B", "octet", "octets", "byte", "bytes"], Dim::Data, 1.0),
    u(&["ko", "kB", "KB", "Ko", "kilooctet", "kilooctets"], Dim::Data, 1e3),
    u(&["Mo", "MB", "mégaoctet", "mégaoctets"], Dim::Data, 1e6),
    u(&["Go", "GB", "gigaoctet", "gigaoctets"], Dim::Data, 1e9),
    u(&["To", "TB", "téraoctet", "téraoctets"], Dim::Data, 1e12),
    u(&["Po", "PB"], Dim::Data, 1e15),
    u(&["Kio", "KiB", "kio", "kiB"], Dim::Data, 1024.0),
    u(&["Mio", "MiB"], Dim::Data, 1_048_576.0),
    u(&["Gio", "GiB"], Dim::Data, 1_073_741_824.0),
    u(&["Tio", "TiB"], Dim::Data, 1_099_511_627_776.0),
    u(&["bit", "bits", "b"], Dim::Data, 0.125),
    u(&["kb", "kbit", "Kb", "kbits"], Dim::Data, 125.0),
    u(&["Mb", "Mbit", "Mbits"], Dim::Data, 125_000.0),
    u(&["Gb", "Gbit", "Gbits"], Dim::Data, 125_000_000.0),
    u(&["m²", "m2", "m^2"], Dim::Area, 1.0),
    u(&["km²", "km2", "km^2"], Dim::Area, 1e6),
    u(&["cm²", "cm2", "cm^2"], Dim::Area, 1e-4),
    u(&["mm²", "mm2", "mm^2"], Dim::Area, 1e-6),
    u(&["ha", "hectare", "hectares"], Dim::Area, 1e4),
    u(&["acre", "acres"], Dim::Area, 4046.8564224),
    u(&["mi²", "mi2", "mi^2"], Dim::Area, 2_589_988.110336),
    u(&["ft²", "ft2", "ft^2"], Dim::Area, 0.09290304),
    u(&["L", "l", "litre", "litres", "liter", "liters"], Dim::Volume, 1.0),
    u(&["mL", "ml", "millilitre", "millilitres"], Dim::Volume, 0.001),
    u(&["cL", "cl", "centilitre", "centilitres"], Dim::Volume, 0.01),
    u(&["dL", "dl", "décilitre", "décilitres"], Dim::Volume, 0.1),
    u(&["m³", "m3", "m^3"], Dim::Volume, 1000.0),
    u(&["cm³", "cm3", "cm^3"], Dim::Volume, 0.001),
    u(&["gal", "gallon", "gallons"], Dim::Volume, 3.785411784),
    u(&["tasse", "tasses", "cup", "cups"], Dim::Volume, 0.2365882365),
    u(&["m/s", "ms-1"], Dim::Speed, 1.0),
    u(&["km/h", "kmh", "kph", "km/hr"], Dim::Speed, 1.0 / 3.6),
    u(&["mph", "mi/h"], Dim::Speed, 0.44704),
    u(&["kn", "kt", "nd", "nœud", "nœuds", "noeud", "noeuds", "knot", "knots"], Dim::Speed, 1852.0 / 3600.0),
    u(&["ft/s", "fps"], Dim::Speed, 0.3048),
    u(&["Pa", "pascal", "pascals"], Dim::Pressure, 1.0),
    u(&["hPa"], Dim::Pressure, 100.0),
    u(&["kPa"], Dim::Pressure, 1000.0),
    u(&["MPa"], Dim::Pressure, 1e6),
    u(&["bar", "bars"], Dim::Pressure, 1e5),
    u(&["mbar"], Dim::Pressure, 100.0),
    u(&["atm"], Dim::Pressure, 101_325.0),
    u(&["psi"], Dim::Pressure, 6894.757293168),
    u(&["mmHg", "Torr", "torr"], Dim::Pressure, 133.322368),
    u(&["J", "joule", "joules"], Dim::Energy, 1.0),
    u(&["kJ"], Dim::Energy, 1000.0),
    u(&["cal"], Dim::Energy, 4.184),
    u(&["kcal", "Cal"], Dim::Energy, 4184.0),
    u(&["Wh"], Dim::Energy, 3600.0),
    u(&["kWh"], Dim::Energy, 3.6e6),
    u(&["BTU", "btu"], Dim::Energy, 1055.056),
    u(&["W", "watt", "watts"], Dim::Power, 1.0),
    u(&["kW"], Dim::Power, 1000.0),
    u(&["MW"], Dim::Power, 1e6),
    u(&["ch", "cv", "cheval", "chevaux"], Dim::Power, 735.49875),
    u(&["hp"], Dim::Power, 745.69987158227),
    u(&["°", "deg", "degré", "degrés", "degree", "degrees"], Dim::Angle, std::f64::consts::PI / 180.0),
    u(&["rad", "radian", "radians"], Dim::Angle, 1.0),
    Unit { names: &["°C", "C", "degC", "celsius", "Celsius"], dim: Dim::Temp, factor: 1.0, offset: KELVIN },
    Unit { names: &["°F", "F", "degF", "fahrenheit", "Fahrenheit"], dim: Dim::Temp, factor: 5.0 / 9.0, offset: KELVIN - 32.0 * 5.0 / 9.0 },
    Unit { names: &["K", "kelvin", "Kelvin"], dim: Dim::Temp, factor: 1.0, offset: 0.0 },
];

const CURRENCIES: &[&str] = &["€", "$", "£", "¥", "eur", "euro", "euros", "usd", "dollar", "dollars", "gbp", "livre sterling", "yen", "chf", "cad"];

fn find_unit(name: &str) -> Result<&'static Unit, CalcError> {
    let name = name.trim();
    if CURRENCIES.iter().any(|c| c.eq_ignore_ascii_case(name)) {
        return Err(CalcError::Currency);
    }
    let lower = name.to_lowercase();
    UNITS
        .iter()
        .find(|unit| unit.names.contains(&name))
        .or_else(|| UNITS.iter().find(|unit| unit.names.iter().any(|n| n.to_lowercase() == lower)))
        .ok_or_else(|| CalcError::UnknownUnit(name.to_string()))
}

/// Converts `value` between two unit names.
pub fn convert(value: f64, from: &str, to: &str) -> Result<f64, CalcError> {
    let (a, b) = (find_unit(from)?, find_unit(to)?);
    if a.dim != b.dim {
        return Err(CalcError::Incompatible);
    }
    finite(((value * a.factor + a.offset) - b.offset) / b.factor)
}

/// Words that separate the quantity from the target unit.
const SEPARATORS: &[&str] = &["en", "to", "vers", "->", "→", "=>"];

/// Splits `12 km en mi` into the quantity and the target unit.
fn split_conversion(src: &str) -> Option<(String, String)> {
    let words: Vec<&str> = src.split_whitespace().collect();
    let at = SEPARATORS
        .iter()
        .find_map(|sep| words.iter().position(|w| w.eq_ignore_ascii_case(sep)))
        // "in" is also the inch: only the last one can be the separator.
        .or_else(|| words.iter().rposition(|w| w.eq_ignore_ascii_case("in")).filter(|&i| i >= 2))?;
    if at == 0 || at + 1 >= words.len() {
        return None;
    }
    Some((words[..at].join(" "), words[at + 1..].join(" ")))
}

/// Number part and unit part of `30 °C`. A missing number counts as one.
fn split_quantity(left: &str) -> Result<(f64, String), CalcError> {
    let unit_at = left.char_indices().find(|&(_, c)| c.is_alphabetic() || "°µ€$£¥\"".contains(c)).map(|(i, _)| i);
    let Some(at) = unit_at else { return Err(CalcError::Syntax) };
    let number = left[..at].trim();
    let value = if number.is_empty() { 1.0 } else { eval(number)? };
    Ok((value, left[at..].trim().to_string()))
}

/// Significant digits kept in a result.
const DIGITS: i32 = 10;

pub fn format_number(v: f64, english: bool) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".to_string();
    }
    let magnitude = v.abs().log10().floor() as i32;
    let text = if !(-6..12).contains(&magnitude) {
        let s = format!("{:.*e}", (DIGITS - 1) as usize, v);
        let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') { mantissa.trim_end_matches('0').trim_end_matches('.') } else { mantissa };
        format!("{mantissa}e{exp}")
    } else {
        let decimals = (DIGITS - 1 - magnitude).clamp(0, 15) as usize;
        let s = format!("{v:.decimals$}");
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    };
    let text = if text == "-0" { "0".to_string() } else { text };
    if english { text } else { text.replace('.', ",") }
}

/// What the calculator shows and copies for `src`.
pub fn compute(src: &str, english: bool) -> Result<String, CalcError> {
    if src.trim().is_empty() {
        return Err(CalcError::Empty);
    }
    if let Some((left, target)) = split_conversion(src) {
        let (value, from) = split_quantity(&left)?;
        let result = convert(value, &from, &target)?;
        return Ok(format!("{} {}", format_number(result, english), target.trim()));
    }
    Ok(format_number(eval(src)?, english))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(s: &str) -> f64 {
        eval(s).unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn precedence_and_brackets() {
        assert_eq!(num("1+2*3"), 7.0);
        assert_eq!(num("(1+2)*3"), 9.0);
        assert_eq!(num("10-4-3"), 3.0);
        assert_eq!(num("100/10/5"), 2.0);
        assert_eq!(num(" 2 * ( 3 + ( 4 - 1 ) ) "), 12.0);
    }

    #[test]
    fn power_is_right_associative_and_binds_tighter_than_minus() {
        assert_eq!(num("2^3^2"), 512.0);
        assert_eq!(num("-2^2"), -4.0);
        assert_eq!(num("2^-1"), 0.5);
        assert_eq!(num("(-2)^2"), 4.0);
    }

    #[test]
    fn decimals_with_a_comma_or_a_dot() {
        assert_eq!(num("1,5+1.5"), 3.0);
        assert_eq!(num(",5*2"), 1.0);
        assert_eq!(num("3.*2"), 6.0);
        assert_eq!(eval("1.2.3"), Err(CalcError::Syntax));
        assert_eq!(eval("1,2,3"), Err(CalcError::Syntax));
        assert_eq!(eval("."), Err(CalcError::Syntax));
    }

    #[test]
    fn unary_signs() {
        assert_eq!(num("-3+5"), 2.0);
        assert_eq!(num("--3"), 3.0);
        assert_eq!(num("+4"), 4.0);
        assert_eq!(num("3*-2"), -6.0);
    }

    #[test]
    fn percent_and_remainder() {
        assert_eq!(num("50%"), 0.5);
        assert_eq!(num("200+10%"), 220.0);
        assert_eq!(num("200-10%"), 180.0);
        assert_eq!(num("200*10%"), 20.0);
        assert_eq!(num("10 % 3"), 1.0);
        assert_eq!(num("10%3"), 1.0);
        assert_eq!(num("(1+1)%"), 0.02);
    }

    #[test]
    fn typographic_operators() {
        assert_eq!(num("6×7"), 42.0);
        assert_eq!(num("84÷2"), 42.0);
        assert_eq!(num("5−2"), 3.0);
    }

    #[test]
    fn errors() {
        assert_eq!(eval(""), Err(CalcError::Empty));
        assert_eq!(eval("   "), Err(CalcError::Empty));
        assert_eq!(eval("1/0"), Err(CalcError::DivZero));
        assert_eq!(eval("5%0"), Err(CalcError::DivZero));
        assert_eq!(eval("1+"), Err(CalcError::Syntax));
        assert_eq!(eval("(1+2"), Err(CalcError::Syntax));
        assert_eq!(eval("1+2)"), Err(CalcError::Syntax));
        assert_eq!(eval("2 3"), Err(CalcError::Syntax));
        assert_eq!(eval("2*x"), Err(CalcError::Syntax));
        assert_eq!(eval("10^400"), Err(CalcError::Range));
        assert_eq!(eval("0^-1"), Err(CalcError::Range));
    }

    #[test]
    fn deep_nesting_is_refused_without_overflowing() {
        let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
        assert_eq!(eval(&deep), Err(CalcError::Syntax));
        let signs = format!("{}1", "-".repeat(5000));
        assert_eq!(eval(&signs), Err(CalcError::Syntax));
        let ok = format!("{}1{}", "(".repeat(20), ")".repeat(20));
        assert_eq!(num(&ok), 1.0);
    }

    #[test]
    fn lengths_and_mass() {
        assert!(near(convert(12.0, "km", "mi").unwrap(), 7.456_454_3));
        assert!(near(convert(1.0, "mi", "km").unwrap(), 1.609344));
        assert!(near(convert(6.0, "ft", "cm").unwrap(), 182.88));
        assert!(near(convert(1.0, "lb", "g").unwrap(), 453.59237));
    }

    #[test]
    fn temperatures_use_offsets() {
        assert!(near(convert(30.0, "°C", "°F").unwrap(), 86.0));
        assert!(near(convert(32.0, "°F", "°C").unwrap(), 0.0));
        assert!(near(convert(0.0, "°C", "K").unwrap(), 273.15));
        assert!(near(convert(-40.0, "°C", "°F").unwrap(), -40.0));
        assert!(near(convert(100.0, "C", "F").unwrap(), 212.0));
    }

    #[test]
    fn data_sizes_decimal_and_binary() {
        assert_eq!(convert(5.0, "Go", "Mo").unwrap(), 5000.0);
        assert_eq!(convert(1.0, "Gio", "Mio").unwrap(), 1024.0);
        assert_eq!(convert(8.0, "bit", "o").unwrap(), 1.0);
        assert_eq!(convert(1.0, "MB", "kB").unwrap(), 1000.0);
        assert_eq!(convert(1.0, "Mb", "kb").unwrap(), 1000.0);
        assert_eq!(convert(1.0, "B", "b").unwrap(), 8.0);
    }

    #[test]
    fn time_and_speed() {
        assert_eq!(convert(2.0, "h", "min").unwrap(), 120.0);
        assert_eq!(convert(1.0, "semaine", "jours").unwrap(), 7.0);
        assert!(near(convert(36.0, "km/h", "m/s").unwrap(), 10.0));
    }

    #[test]
    fn unit_names_fall_back_to_any_case() {
        assert_eq!(convert(1.0, "go", "mo").unwrap(), 1000.0);
        assert_eq!(convert(1.0, "KM", "m").unwrap(), 1000.0);
    }

    #[test]
    fn refused_conversions() {
        assert_eq!(convert(1.0, "km", "kg"), Err(CalcError::Incompatible));
        assert_eq!(convert(1.0, "foo", "km"), Err(CalcError::UnknownUnit("foo".into())));
        assert_eq!(convert(1.0, "€", "$"), Err(CalcError::Currency));
        assert_eq!(convert(1.0, "m", "EUR"), Err(CalcError::Currency));
    }

    #[test]
    fn conversion_sentences() {
        assert_eq!(compute("12 km en mi", false).unwrap(), "7,456454307 mi");
        assert_eq!(compute("30 °C en °F", false).unwrap(), "86 °F");
        assert_eq!(compute("5 Go en Mo", false).unwrap(), "5000 Mo");
        assert_eq!(compute("2 h en min", false).unwrap(), "120 min");
        assert_eq!(compute("2 h to min", true).unwrap(), "120 min");
        assert_eq!(compute("1,5 h en min", false).unwrap(), "90 min");
        assert_eq!(compute("(1+2)*2 km en m", false).unwrap(), "6000 m");
        assert_eq!(compute("km en m", false).unwrap(), "1000 m");
        assert_eq!(compute("30 km/h en m/s", false).unwrap(), "8,333333333 m/s");
        assert_eq!(compute("12 in en cm", true).unwrap(), "30.48 cm");
        assert_eq!(compute("12 km in mi", true).unwrap(), compute("12 km en mi", true).unwrap());
        assert_eq!(compute("100 € en $", false), Err(CalcError::Currency));
        assert_eq!(compute("12 en km", false), Err(CalcError::Syntax));
    }

    #[test]
    fn plain_sums_go_through_compute() {
        assert_eq!(compute("1+2", false).unwrap(), "3");
        assert_eq!(compute("1/3", false).unwrap(), "0,3333333333");
        assert_eq!(compute("1/3", true).unwrap(), "0.3333333333");
        assert_eq!(compute("0,1+0,2", false).unwrap(), "0,3");
        assert_eq!(compute("", false), Err(CalcError::Empty));
    }

    #[test]
    fn numbers_are_trimmed_and_shortened() {
        assert_eq!(format_number(0.0, false), "0");
        assert_eq!(format_number(-0.0, false), "0");
        assert_eq!(format_number(1234.5, false), "1234,5");
        assert_eq!(format_number(-2.0, true), "-2");
        assert_eq!(format_number(0.1 + 0.2, true), "0.3");
        assert_eq!(format_number(1e15, true), "1e15");
        assert_eq!(format_number(1.5e-9, true), "1.5e-9");
        assert_eq!(format_number(123_456_789_012.0, true), "123456789012");
    }
}
