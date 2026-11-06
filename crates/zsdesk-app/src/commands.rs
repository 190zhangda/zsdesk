/// Keyboard and chrome commands handled by the Explorer window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    Back,
    Forward,
    Up,
    FocusAddress,
    Refresh,
    AddBookmark,
    ReopenTab,
    FocusSearch,
    NewFolder,
    CopyPath,
    TogglePreview,
    ToggleDetails,
    /// Ctrl+1 … Ctrl+9 (9 selects the last tab, like browsers).
    SelectTab(u8),
}

impl Command {
    pub const fn id(self) -> u16 {
        match self {
            Self::NewTab => 2001,
            Self::CloseTab => 2002,
            Self::NextTab => 2003,
            Self::PreviousTab => 2004,
            Self::Back => 2005,
            Self::Forward => 2006,
            Self::Up => 2007,
            Self::FocusAddress => 2008,
            Self::Refresh => 2009,
            Self::AddBookmark => 2010,
            Self::ReopenTab => 2011,
            Self::FocusSearch => 2012,
            Self::NewFolder => 2013,
            Self::CopyPath => 2014,
            Self::TogglePreview => 2015,
            Self::ToggleDetails => 2016,
            Self::SelectTab(n) => 2020 + n as u16,
        }
    }

    pub const fn from_id(id: u16) -> Option<Self> {
        match id {
            2001 => Some(Self::NewTab),
            2002 => Some(Self::CloseTab),
            2003 => Some(Self::NextTab),
            2004 => Some(Self::PreviousTab),
            2005 => Some(Self::Back),
            2006 => Some(Self::Forward),
            2007 => Some(Self::Up),
            2008 => Some(Self::FocusAddress),
            2009 => Some(Self::Refresh),
            2010 => Some(Self::AddBookmark),
            2011 => Some(Self::ReopenTab),
            2012 => Some(Self::FocusSearch),
            2013 => Some(Self::NewFolder),
            2014 => Some(Self::CopyPath),
            2015 => Some(Self::TogglePreview),
            2016 => Some(Self::ToggleDetails),
            2021..=2029 => Some(Self::SelectTab((id - 2020) as u8)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Command;
    #[test]
    fn ids_round_trip() {
        for c in [Command::NewTab, Command::NewFolder, Command::ToggleDetails, Command::SelectTab(1), Command::SelectTab(9)] {
            assert_eq!(Command::from_id(c.id()), Some(c));
        }
        assert_eq!(Command::from_id(2020), None);
    }
}
