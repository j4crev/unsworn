//! Diagnostics: errors and warnings with a location and an optional fix.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    /// Where the problem is, e.g. `unsworn.toml:12 route[2].handler`.
    pub location: String,
    pub message: String,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn error(location: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            level: Level::Error,
            location: location.into(),
            message: message.into(),
            hint: None,
        }
    }

    pub fn warning(location: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            level: Level::Warning,
            location: location.into(),
            message: message.into(),
            hint: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = match self.level {
            Level::Error => "✗",
            Level::Warning => "!",
        };
        if self.location.is_empty() {
            write!(f, "{mark} {}", self.message)?;
        } else {
            write!(f, "{mark} {}: {}", self.location, self.message)?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n  → {hint}")?;
        }
        Ok(())
    }
}

/// Collects diagnostics while compiling a project.
#[derive(Debug, Default)]
pub struct Diagnostics {
    pub items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    pub fn error(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.push(Diagnostic::error(location, message));
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| d.is_error())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter().filter(|d| !d.is_error())
    }
}
