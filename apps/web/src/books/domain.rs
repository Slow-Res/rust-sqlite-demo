//! Validated book types. Pure logic: no IO, no HTTP, no SQL.

use library_shared::BookInput;

use crate::error::FieldErrors;

const MAX_TITLE_CHARS: usize = 300;
const MAX_AUTHOR_CHARS: usize = 200;
const MAX_COPIES: i64 = 1000;

/// A syntactically valid ISBN-10 or ISBN-13, stored without hyphens or spaces
/// (and with an uppercase `X` check digit).
///
/// This is the "parse, don't validate" pattern: the only way to get an `Isbn`
/// is [`Isbn::parse`], so any function that receives one can trust it without
/// re-checking. The inner `String` is private to keep it that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Isbn(String);

impl Isbn {
    /// Strip spaces and hyphens, then check the length and checksum.
    /// The error is a short message suitable for a field error.
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        let normalized: String = input
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let valid = match normalized.len() {
            10 => isbn10_is_valid(&normalized),
            13 => isbn13_is_valid(&normalized),
            _ => return Err("must be a valid ISBN-10 or ISBN-13"),
        };
        if valid {
            Ok(Self(normalized))
        } else {
            Err("must be a valid ISBN-10 or ISBN-13")
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Nine digits plus a digit or `X` (= 10); weights 10 down to 1; sum divisible by 11.
fn isbn10_is_valid(s: &str) -> bool {
    let mut sum = 0;
    for (i, (c, weight)) in s.chars().zip((1..=10u32).rev()).enumerate() {
        let value = match c {
            'X' if i == 9 => 10,
            _ => match c.to_digit(10) {
                Some(d) => d,
                None => return false,
            },
        };
        sum += value * weight;
    }
    sum % 11 == 0
}

/// Thirteen digits; weights alternate 1, 3; sum divisible by 10.
fn isbn13_is_valid(s: &str) -> bool {
    let mut sum = 0;
    for (i, c) in s.chars().enumerate() {
        let Some(d) = c.to_digit(10) else {
            return false;
        };
        sum += d * if i % 2 == 0 { 1 } else { 3 };
    }
    sum % 10 == 0
}

/// Book data that has passed validation, ready to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFields {
    pub title: String,
    pub author: String,
    pub isbn: Option<Isbn>,
    pub published_year: Option<i32>,
    pub total_copies: i64,
}

impl BookFields {
    /// Validate a request body, collecting *every* bad field so the client can
    /// show all problems at once. `current_year` comes from the injected clock
    /// so the rule is testable.
    pub fn parse(input: BookInput, current_year: i32) -> Result<Self, FieldErrors> {
        let mut errors = FieldErrors::new();

        let title = input.title.trim();
        match title.chars().count() {
            0 => errors.add("title", "must not be empty"),
            n if n > MAX_TITLE_CHARS => errors.add("title", "must be at most 300 characters"),
            _ => {}
        }

        let author = input.author.trim();
        match author.chars().count() {
            0 => errors.add("author", "must not be empty"),
            n if n > MAX_AUTHOR_CHARS => errors.add("author", "must be at most 200 characters"),
            _ => {}
        }

        // Blank means "no ISBN", not "invalid ISBN".
        let isbn = match input.isbn.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => match Isbn::parse(raw) {
                Ok(isbn) => Some(isbn),
                Err(message) => {
                    errors.add("isbn", message);
                    None
                }
            },
        };

        if let Some(year) = input.published_year
            && !(1..=current_year + 1).contains(&year)
        {
            errors.add("published_year", "must be a valid year");
        }

        if !(1..=MAX_COPIES).contains(&input.total_copies) {
            errors.add("total_copies", "must be between 1 and 1000");
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self {
            title: title.to_owned(),
            author: author.to_owned(),
            isbn,
            published_year: input.published_year,
            total_copies: input.total_copies,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> BookInput {
        BookInput {
            title: "Dune".into(),
            author: "Frank Herbert".into(),
            isbn: None,
            published_year: Some(1965),
            total_copies: 3,
        }
    }

    fn field_names(errors: &FieldErrors) -> Vec<&str> {
        errors.iter().map(|(f, _)| f).collect()
    }

    #[test]
    fn valid_isbn13() {
        assert_eq!(
            Isbn::parse("978-0-13-110362-7").unwrap().as_str(),
            "9780131103627"
        );
        assert_eq!(
            Isbn::parse("978 0 13 110362 7").unwrap().as_str(),
            "9780131103627"
        );
    }

    #[test]
    fn valid_isbn10_including_x() {
        assert_eq!(Isbn::parse("0-306-40615-2").unwrap().as_str(), "0306406152");
        assert_eq!(Isbn::parse("0-8044-2957-X").unwrap().as_str(), "080442957X");
        assert_eq!(Isbn::parse("0-8044-2957-x").unwrap().as_str(), "080442957X");
    }

    #[test]
    fn rejects_bad_checksum() {
        assert!(Isbn::parse("0306406153").is_err());
        assert!(Isbn::parse("9780131103628").is_err());
    }

    #[test]
    fn rejects_wrong_length_and_characters() {
        assert!(Isbn::parse("123456789").is_err());
        assert!(Isbn::parse("978013110362").is_err());
        assert!(Isbn::parse("").is_err());
        assert!(Isbn::parse("03064X6152").is_err()); // X only allowed last
        assert!(Isbn::parse("978013110362X").is_err()); // no X in ISBN-13
        assert!(Isbn::parse("abcdefghij").is_err());
    }

    #[test]
    fn parse_trims_and_keeps_valid_input() {
        let mut raw = input();
        raw.title = "  Dune  ".into();
        raw.isbn = Some(" 978-0-13-110362-7 ".into());
        let fields = BookFields::parse(raw, 2027).unwrap();
        assert_eq!(fields.title, "Dune");
        assert_eq!(fields.isbn.unwrap().as_str(), "9780131103627");
    }

    #[test]
    fn blank_isbn_means_none() {
        let mut raw = input();
        raw.isbn = Some("   ".into());
        assert_eq!(BookFields::parse(raw, 2027).unwrap().isbn, None);
    }

    #[test]
    fn length_limits_count_characters_after_trimming() {
        let mut raw = input();
        raw.title = format!(" {} ", "é".repeat(300));
        raw.author = "a".repeat(200);
        assert!(BookFields::parse(raw.clone(), 2027).is_ok());
        raw.title = "é".repeat(301);
        raw.author = "a".repeat(201);
        let errors = BookFields::parse(raw, 2027).unwrap_err();
        assert_eq!(field_names(&errors), ["author", "title"]);
    }

    #[test]
    fn blank_title_and_author_are_rejected() {
        let mut raw = input();
        raw.title = "   ".into();
        raw.author = String::new();
        let errors = BookFields::parse(raw, 2027).unwrap_err();
        assert_eq!(field_names(&errors), ["author", "title"]);
    }

    #[test]
    fn year_bounds_are_one_to_next_year() {
        let with_year = |y| BookInput {
            published_year: Some(y),
            ..input()
        };
        assert!(BookFields::parse(with_year(1), 2027).is_ok());
        assert!(BookFields::parse(with_year(2028), 2027).is_ok());
        assert!(BookFields::parse(with_year(0), 2027).is_err());
        assert!(BookFields::parse(with_year(2029), 2027).is_err());
    }

    #[test]
    fn copies_bounds() {
        let with_copies = |c| BookInput {
            total_copies: c,
            ..input()
        };
        assert!(BookFields::parse(with_copies(1), 2027).is_ok());
        assert!(BookFields::parse(with_copies(1000), 2027).is_ok());
        assert!(BookFields::parse(with_copies(0), 2027).is_err());
        assert!(BookFields::parse(with_copies(1001), 2027).is_err());
    }

    #[test]
    fn collects_every_bad_field() {
        let raw = BookInput {
            title: String::new(),
            author: String::new(),
            isbn: Some("nope".into()),
            published_year: Some(0),
            total_copies: 0,
        };
        let errors = BookFields::parse(raw, 2027).unwrap_err();
        assert_eq!(
            field_names(&errors),
            ["author", "isbn", "published_year", "title", "total_copies"]
        );
    }
}
