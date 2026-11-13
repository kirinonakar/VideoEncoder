use crate::{LocalizedText, MainWindow};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, fmt};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Language {
    En,
    Ko,
}

impl Language {
    pub fn system() -> Self {
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

            // The lower ten bits of a LANGID identify the primary language.
            const LANG_KOREAN: u16 = 0x12;
            if unsafe { GetUserDefaultUILanguage() } & 0x03ff == LANG_KOREAN {
                return Self::Ko;
            }
        }
        Self::En
    }

    pub fn current(ui: &MainWindow) -> Self {
        if ui.get_korean() { Self::Ko } else { Self::En }
    }

    pub fn select<T>(self, english: T, korean: T) -> T {
        match self {
            Self::En => english,
            Self::Ko => korean,
        }
    }
}

impl LocalizedText {
    pub fn new(
        english: impl Into<slint::SharedString>,
        korean: impl Into<slint::SharedString>,
    ) -> Self {
        Self {
            english: english.into(),
            korean: korean.into(),
        }
    }

    pub fn same(text: impl Into<slint::SharedString>) -> Self {
        let text = text.into();
        Self::new(text.clone(), text)
    }
}

macro_rules! message {
    ($english:literal, $korean:literal $(, $($args:tt)*)?) => {
        $crate::LocalizedText::new(format!($english $(, $($args)*)?), format!($korean $(, $($args)*)?))
    };
}
pub(crate) use message;

thread_local! {
    // Error contexts retain both translations through the worker and error chain.
    static DIAGNOSTIC_LANGUAGE: Cell<Language> = const { Cell::new(Language::Ko) };
}

impl fmt::Display for LocalizedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            DIAGNOSTIC_LANGUAGE
                .get()
                .select(&self.english, &self.korean),
        )
    }
}

impl std::error::Error for LocalizedText {}

pub fn diagnostic(error: &impl fmt::Display) -> LocalizedText {
    struct RestoreLanguage(Language);
    impl Drop for RestoreLanguage {
        fn drop(&mut self) {
            DIAGNOSTIC_LANGUAGE.set(self.0);
        }
    }

    let _restore = RestoreLanguage(DIAGNOSTIC_LANGUAGE.replace(Language::En));
    let english = format!("{error:#}");
    DIAGNOSTIC_LANGUAGE.set(Language::Ko);
    LocalizedText::new(english, format!("{error:#}"))
}
