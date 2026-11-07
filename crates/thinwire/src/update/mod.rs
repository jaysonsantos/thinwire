//! macOS update check. The frame reads a [`Prompt`] and sends an [`Action`].

mod machine;

/// What the frame may draw. No URL, path, signature, or feed type is in here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Prompt {
    Quiet,
    Offer(OfferView),
    Installing,
    /// `Installing` reached `Armed`. `logic` sends `ViewportCommand::Close`.
    QuitToInstall,
    Failed(PlainError),
}

/// Version line and notes for the modal. Built only by [`project`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OfferView {
    pub version: String,
    pub notes: String,
}

/// A sentence the modal may show. The UI does not format errors itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlainError {
    pub text: &'static str,
    pub retry: bool,
}

pub(super) const CHECK_FAILED: PlainError = PlainError {
    text: "Couldn't check for updates.",
    retry: true,
};

pub(super) const BAD_SIGNATURE: PlainError = PlainError {
    text: "This download isn't a signed Thinwire update.",
    retry: true,
};

pub(super) const SWAP_FAILED: PlainError = PlainError {
    text: "Couldn't install the update. Thinwire is unchanged.",
    retry: true,
};

pub(super) const MOVE_TO_APPLICATIONS: PlainError = PlainError {
    text: "Move Thinwire.app to the Applications folder and open it from there.",
    retry: false,
};

pub(super) const NOT_WRITABLE: PlainError = PlainError {
    text: "Thinwire couldn't replace the app in its folder. Move it somewhere you can write to, then try again.",
    retry: true,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Install,
    Later,
    Dismiss,
    Retry,
}

fn project(state: &machine::Update) -> Prompt {
    match state {
        machine::Update::Dormant
        | machine::Update::Checking { .. }
        | machine::Update::Quiet { .. } => Prompt::Quiet,
        machine::Update::Offer { offer, .. } => Prompt::Offer(offer.view()),
        machine::Update::Installing {
            phase: machine::InstallPhase::Armed,
            ..
        } => Prompt::QuitToInstall,
        machine::Update::Installing { .. } => Prompt::Installing,
        machine::Update::Failed { error, .. } => Prompt::Failed(*error),
    }
}
