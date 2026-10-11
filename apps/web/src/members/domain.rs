//! Validated member types. Pure logic: no IO, no HTTP, no SQL.

use library_shared::MemberInput;

use crate::error::FieldErrors;

const MAX_NAME_CHARS: usize = 200;
const MAX_EMAIL_CHARS: usize = 254;

/// A syntactically plausible email address, trimmed and lowercased so that
/// uniqueness in the database is effectively case-insensitive.
///
/// Like `Isbn`, the only constructor is [`Email::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email(String);

impl Email {
    /// Trim and lowercase, then require: at most 254 characters, exactly one
    /// `@`, a non-empty local part, and a domain containing a `.` that is
    /// neither its first nor its last character.
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        const INVALID: &str = "must be a valid email address";
        let email = input.trim().to_lowercase();
        if email.chars().count() > MAX_EMAIL_CHARS {
            return Err("must be at most 254 characters");
        }
        let mut parts = email.split('@');
        let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(INVALID);
        };
        let dot_inside_domain =
            domain.find('.').is_some() && !domain.starts_with('.') && !domain.ends_with('.');
        if local.is_empty() || !dot_inside_domain {
            return Err(INVALID);
        }
        Ok(Self(email))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Member data that has passed validation, ready to be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFields {
    pub name: String,
    pub email: Email,
}

impl MemberFields {
    /// Validate a request body, collecting every bad field.
    pub fn parse(input: MemberInput) -> Result<Self, FieldErrors> {
        let mut errors = FieldErrors::new();

        let name = input.name.trim();
        match name.chars().count() {
            0 => errors.add("name", "must not be empty"),
            n if n > MAX_NAME_CHARS => errors.add("name", "must be at most 200 characters"),
            _ => {}
        }

        let email = match Email::parse(&input.email) {
            Ok(email) => Some(email),
            Err(message) => {
                errors.add("email", message);
                None
            }
        };

        match email {
            Some(email) if errors.is_empty() => Ok(Self {
                name: name.to_owned(),
                email,
            }),
            _ => Err(errors),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> MemberInput {
        MemberInput {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        }
    }

    fn field_names(errors: &FieldErrors) -> Vec<&str> {
        errors.iter().map(|(f, _)| f).collect()
    }

    #[test]
    fn email_is_trimmed_and_lowercased() {
        assert_eq!(
            Email::parse("  Grace@Example.COM ").unwrap().as_str(),
            "grace@example.com"
        );
    }

    #[test]
    fn email_accepts_subdomains() {
        assert!(Email::parse("a@mail.example.co.uk").is_ok());
    }

    #[test]
    fn email_rejects_bad_shapes() {
        for bad in [
            "",
            "plain",
            "@example.com",
            "a@",
            "a@b",
            "a@.com",
            "a@example.",
            "a@@example.com",
            "a@b@example.com",
        ] {
            assert!(Email::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn email_length_limit_is_254_characters() {
        let at_limit = format!("{}@example.com", "a".repeat(254 - "@example.com".len()));
        assert_eq!(at_limit.chars().count(), 254);
        assert!(Email::parse(&at_limit).is_ok());
        assert!(Email::parse(&format!("a{at_limit}")).is_err());
    }

    #[test]
    fn parse_trims_name_and_normalizes_email() {
        let fields = MemberFields::parse(MemberInput {
            name: "  Grace  ".into(),
            email: " Grace@Example.com ".into(),
        })
        .unwrap();
        assert_eq!(fields.name, "Grace");
        assert_eq!(fields.email.as_str(), "grace@example.com");
    }

    #[test]
    fn name_limits_count_characters_after_trimming() {
        let mut raw = input();
        raw.name = format!(" {} ", "é".repeat(200));
        assert!(MemberFields::parse(raw.clone()).is_ok());
        raw.name = "é".repeat(201);
        assert_eq!(
            field_names(&MemberFields::parse(raw).unwrap_err()),
            ["name"]
        );
        let mut raw = input();
        raw.name = "   ".into();
        assert_eq!(
            field_names(&MemberFields::parse(raw).unwrap_err()),
            ["name"]
        );
    }

    #[test]
    fn collects_every_bad_field() {
        let errors = MemberFields::parse(MemberInput {
            name: String::new(),
            email: "nope".into(),
        })
        .unwrap_err();
        assert_eq!(field_names(&errors), ["email", "name"]);
    }
}
