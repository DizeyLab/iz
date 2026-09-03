//! Per-user UI strings. Part A wires `board.rs`/`detail.rs`; part B appends
//! `auth.rs`/`rules.rs`/`logs.rs`/`pages.rs`/`settings.rs`'s keys to the same
//! `Key` enum and `t` match, nothing else to change here. Mail bodies in
//! `iz-core` stay English — a compose has no viewer to pick a language for.

/// A user's stored `language` column ('en' default). Anything not `"tr"` is
/// English — an unrecognized code is not a refusal, just the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Tr,
}

impl Lang {
    pub fn from_code(code: &str) -> Lang {
        match code {
            "tr" => Lang::Tr,
            _ => Lang::En,
        }
    }

    /// The `<html lang>` value.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Tr => "tr",
        }
    }
}

/// One variant per UI phrase. A typo'd key fails to compile rather than
/// falling through to nothing at render time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    NewTask,
    Title,
    Cancel,
    SomethingWentWrong,
    BackToBoard,
    NothingAtThisAddress,
    NobodyAssignedAria,
    NobodyAssignedTitle,
    Open,
    Move,
    Delete,
    Settings,
    Sort,
    SortDeadline,
    SortCreated,
    SortTitle,
    RenameThisTask,
    Save,
    EditTheDescription,
    ChangeTheDeadline,
    PutSomeoneOnThisTask,
    LinkAnotherTask,
    LinkATask,
    Direction,
    BlocksThisTask,
    WaitsOnThisTask,
    Link,
    RemoveThisLink,
    OnAComment,
    RemoveThisFile,
    Download,
    Play,
    Pause,
    CloseTheFile,
    ThisFileWillNotOpen,
    Previous,
    Next,
    Esc,
    CloseThisTask,
    Status,
    Assignees,
    Deadline,
    Description,
    Dependencies,
    TabTask,
    Files,
    File,
    Comments,
    TabMail,
    WriteAComment,
    Comment,
    Activity,
    Notifications,
    Retry,
    DeleteTask,
    CommentGoesWithIt,
    CommentsGoWithIt,
    DependencyStopsApplying,
    DependenciesStopApplying,
    StopsBeingBlocked,
    ThisTaskCannotBeDeleted,
    Close,
    Blocks,
    BlockedBy,
    Overdue,
    Blocked,

    // Topbar nav / chrome shared by board.rs, rules.rs, logs.rs, settings.rs.
    NavBoard,
    NavMailRules,
    NavTags,
    NavLogs,
    NavSettings,
    AdminOnly,

    // tags.rs — Project/AllTags also serve board.rs's filter and detail.rs.
    NewTag,
    NameLabel,
    AddTag,
    SaveTag,
    EditThisTag,
    DeleteThisTag,
    MoveTagUp,
    MoveTagDown,
    Project,
    AllTags,

    // settings.rs
    YourProfile,
    Remove,
    DisplayNameLabel,
    EmailLabel,
    TimezoneLabel,
    ThemeLabel,
    LightOption,
    DarkOption,
    UiLabel,
    LanguageLabel,
    Saved,
    SignOut,
    OutgoingMail,
    Connected,
    NotConfiguredChip,
    Unchecked,
    Refused,
    CheckConnection,
    NotConnectedNote,
    SmtpHostLabel,
    PortLabel,
    UsernameLabel,
    PasswordLabel,
    PasswordSetPlaceholder,
    PasswordNeededPlaceholder,
    FromNameLabel,
    FromAddressLabel,
    SendTestMail,
    WorkspaceLimits,
    AttachmentLimitLabel,
    PhotoLimitLabel,
    AllowedFileTypesLabel,
    Members,
    Message,
    Everyone,
    Recipient,
    Subject,
    Body,
    Send,
    NameCol,
    AddressCol,
    RoleCol,
    AccountCol,
    OwnerStatus,
    InvitedStatus,
    ActiveStatus,
    You,
    ResendMail,
    SendSigninLink,
    RoleMemberOption,
    RoleViewerOption,
    RoleAdminOption,
    AddMember,
    SmtpHostRequired,
    SmtpHostInvalid,
    PortInvalid,
    SmtpUsernameRequired,
    PasswordNeededFirstTime,
    NotFromAddress,
    NotAnOrigin,
    LinkAddressLabel,
    SenderNotConfiguredYet,

    // pages.rs
    LinkExpiredTitle,
    SetupTitle,
    SetupSub,
    YourNameLabel,
    CreateWorkspace,
    SignInTitle,
    SignInSub,
    SignInButton,
    PickPasswordTitle,
    AdminMadeYouAnAccount,
    SigningInAsLabel,
    NewPasswordLabel,
    SetPasswordAndSignIn,
    CurrentPasswordLabel,
    ChangePassword,

    // rules.rs
    MailRules,
    EditLabel,
    NoSenderConnectedPrefix,
    NewRule,
    WhenLabel,
    ColumnLabel,
    SendLabel,
    ToLabel,
    BodyLabel,
    TaskDetails,
    AddRule,
    SaveRule,
    EditThisRule,
    DeleteThisRule,
    SwitchRuleOff,
    SwitchRuleOn,
    NeverFired,
    NoRulesYet,
    TriggerStatusBecomes,
    TriggerUnblocked,
    TriggerCreated,
    TriggerAssigned,
    TriggerUnassigned,
    TriggerCommented,
    TriggerDeadlineSet,
    TriggerDeadlineCleared,
    TriggerRetitled,
    TriggerLinked,
    TriggerUnlinked,
    TriggerDeleted,
    AudienceAssignees,
    AudienceBoard,
    AudienceCreator,
    WhenStatusBecomes,
    WhenStatusChanges,
    AnyColumn,
    WhenTaskStopsBeingBlocked,
    WhenTaskCreated,
    WhenSomeoneAssigned,
    WhenSomeoneUnassigned,
    WhenCommentWritten,
    WhenDeadlineSet,
    WhenDeadlineRemoved,
    WhenTaskRenamed,
    WhenTaskLinked,
    WhenTaskUnlinked,
    WhenTaskDeleted,
    BlockedWord,
    ColumnGone,
    AudAssignees,
    AudEveryoneOnBoard,
    AudItsCreator,
    SendConnector,
    ToConnector,

    // logs.rs
    Logs,
    MailQueue,
    MailDecisions,
    WorkspaceActivity,
    NothingOwed,
    NoDecisionsYet,
    NothingYet,
    NoRetry,
    NextTry,
    DueNow,
    Older,
    Newer,
    QueueStatePending,
    QueueStateHeld,
    QueueStateFailed,
    QueueStateSent,
    QueueStateAbandoned,
    RuleGone,
    TaskGoneLabel,
    OutcomeQueued,
    OutcomeAlreadyQueued,
    OutcomeNoRecipients,
    OutcomeNotMatched,
    OutcomeRuleOff,
    OutcomeTaskGone,
    ActCreated,
    ActRetitled,
    ActDescribed,
    ActDeadlineCleared,
    ActClockCleared,
    ActDeleted,
    ActCommented,
    ActWorkspaceClaimed,
    ActJoined,
    ActSignedIn,
    ActSignedOut,
    ActPasswordChanged,
    ActProfileSaved,
    ActSenderSaved,
    ActLimitsSaved,
    ActTestMailSent,
    ActMessageSent,
    ActSendRetried,
    ActPhotoSaved,
    ActPhotoRemoved,
    ActSenderChecked,
    PWCurrentWrong,
    PWIsCurrent,
    PWTooShort,
    PWLooksLikeYou,
    PasswordSaved,
    UnblockedWord,
    AColumn,
    AudienceEmpty,
    AudienceActorOnly,
    NotAStatusCrossing,
    NotAnUnblockedEvent,
    TheSystem,
    NoDeadline,
    NoTasks,
    NoDescription,
    NoDependencies,
    Subtasks,
    SubtaskGoesWithIt,
    SubtasksGoWithIt,
    NoSubtasks,
    NewSubtask,
    AddSubtask,
    MakeAPart,
    ReleaseThisPart,
    ExistingTask,
    DonePrefix,
    Clear,
    Today,
    PreviousMonth,
    NextMonth,
    WeekdayInitials,
    MonthJanuary,
    MonthFebruary,
    MonthMarch,
    MonthApril,
    MonthMay,
    MonthJune,
    MonthJuly,
    MonthAugust,
    MonthSeptember,
    MonthOctober,
    MonthNovember,
    MonthDecember,
    All,
    LogSystem,
    Newest,
    Oldest,
    From,
    To,
    // people.rs
    TasksOpenLabel,
    TasksDoneLabel,
    TasksCreatedLabel,
    CommentsLabel,
    RecentActivity,
    JoinedLabel,
    LastSeenLabel,
    InvitedByLabel,
    MailBatchLabel,
    ReminderMinutesLabel,
    ClockHour,
    ClockMinute,
    Search,
    NoMatches,
    ForgotIt,
    ForgotTitle,
    ForgotSend,
    ForgotSent,
    ResetTitle,
    ResetLinkDead,
    ActResetRequested,
    ActResetDone,
    Assignee,
    ActReminded,
    Nobody,
}

/// The lone templated phrase — a name in the middle reads worse as two
/// concatenated key lookups than as one function, in either language.
pub fn take_off_this_task(lang: Lang, name: &str) -> String {
    match lang {
        Lang::En => format!("Take {name} off this task"),
        Lang::Tr => format!("{name} kişisini bu görevden çıkar"),
    }
}

/// Who made an invited member's account, when that is still knowable.
pub fn made_you_an_account(lang: Lang, admin_name: &str) -> String {
    match lang {
        Lang::En => format!("{admin_name} made you an account."),
        Lang::Tr => format!("{admin_name} senin için hesap oluşturdu."),
    }
}

/// The phrase a key names, in a user's language.
pub fn t(lang: Lang, key: Key) -> &'static str {
    use Key::*;
    use Lang::*;
    match (key, lang) {
        (NewTask, En) => "New task",
        (NewTask, Tr) => "Yeni görev",
        (Title, En) => "TITLE",
        (Title, Tr) => "BAŞLIK",
        (Cancel, En) => "Cancel",
        (Cancel, Tr) => "Vazgeç",
        (SomethingWentWrong, En) => "Something went wrong.",
        (SomethingWentWrong, Tr) => "Bir şeyler ters gitti.",
        (BackToBoard, En) => "Back to the board",
        (BackToBoard, Tr) => "Panoya dön",
        (NothingAtThisAddress, En) => "Nothing at this address.",
        (NothingAtThisAddress, Tr) => "Bu adreste bir şey yok.",
        (NobodyAssignedAria, En) => "nobody assigned",
        (NobodyAssignedAria, Tr) => "kimse atanmadı",
        (NobodyAssignedTitle, En) => "Nobody assigned",
        (NobodyAssignedTitle, Tr) => "Kimse atanmadı",
        (Open, En) => "Open",
        (Open, Tr) => "Aç",
        (Move, En) => "Move",
        (Move, Tr) => "Taşı",
        (Delete, En) => "Delete",
        (Delete, Tr) => "Sil",
        (Settings, En) => "Settings",
        (Settings, Tr) => "Ayarlar",
        (Sort, En) => "Sort",
        (Sort, Tr) => "Sırala",
        (SortDeadline, En) => "Deadline",
        (SortDeadline, Tr) => "Son tarih",
        (SortCreated, En) => "Created",
        (SortCreated, Tr) => "Oluşturulma",
        (SortTitle, En) => "Title",
        (SortTitle, Tr) => "Başlık",
        (RenameThisTask, En) => "Rename this task",
        (RenameThisTask, Tr) => "Bu görevi yeniden adlandır",
        (Save, En) => "Save",
        (Save, Tr) => "Kaydet",
        (EditTheDescription, En) => "Edit the description",
        (EditTheDescription, Tr) => "Açıklamayı düzenle",
        (ChangeTheDeadline, En) => "Change the deadline",
        (ChangeTheDeadline, Tr) => "Son tarihi değiştir",
        (PutSomeoneOnThisTask, En) => "Put someone on this task",
        (PutSomeoneOnThisTask, Tr) => "Bu göreve birini ata",
        (LinkAnotherTask, En) => "Link another task",
        (LinkAnotherTask, Tr) => "Başka bir görevi bağla",
        (LinkATask, En) => "Link a task",
        (LinkATask, Tr) => "Görev bağla",
        (Direction, En) => "DIRECTION",
        (Direction, Tr) => "YÖN",
        (BlocksThisTask, En) => "blocks this task",
        (BlocksThisTask, Tr) => "bu görevi engelliyor",
        (WaitsOnThisTask, En) => "waits on this task",
        (WaitsOnThisTask, Tr) => "bu görevi bekliyor",
        (Link, En) => "Link",
        (Link, Tr) => "Bağla",
        (RemoveThisLink, En) => "Remove this link",
        (RemoveThisLink, Tr) => "Bu bağlantıyı kaldır",
        (OnAComment, En) => "on a comment",
        (OnAComment, Tr) => "bir yoruma ekli",
        (RemoveThisFile, En) => "Remove this file",
        (RemoveThisFile, Tr) => "Bu dosyayı kaldır",
        (Download, En) => "Download",
        (Download, Tr) => "İndir",
        (Play, En) => "Play",
        (Play, Tr) => "Oynat",
        (Pause, En) => "Pause",
        (Pause, Tr) => "Duraklat",
        (CloseTheFile, En) => "Close the file",
        (CloseTheFile, Tr) => "Dosyayı kapat",
        (ThisFileWillNotOpen, En) => "This file will not open.",
        (ThisFileWillNotOpen, Tr) => "Bu dosya açılmıyor.",
        (Previous, En) => "Previous",
        (Previous, Tr) => "Önceki",
        (Next, En) => "Next",
        (Next, Tr) => "Sonraki",
        (Esc, En) => "esc",
        (Esc, Tr) => "esc",
        (CloseThisTask, En) => "Close this task",
        (CloseThisTask, Tr) => "Bu görevi kapat",
        (Status, En) => "STATUS",
        (Status, Tr) => "DURUM",
        (Assignees, En) => "ASSIGNEES",
        (Assignees, Tr) => "ATANANLAR",
        (Deadline, En) => "DEADLINE",
        (Deadline, Tr) => "SON TARİH",
        (Description, En) => "DESCRIPTION",
        (Description, Tr) => "AÇIKLAMA",
        (Dependencies, En) => "DEPENDENCIES",
        (Dependencies, Tr) => "BAĞIMLILIKLAR",
        (TabTask, En) => "Task",
        (TabTask, Tr) => "Görev",
        (Files, En) => "FILES",
        (Files, Tr) => "DOSYALAR",
        (File, En) => "File",
        (File, Tr) => "Dosya",
        (Comments, En) => "COMMENTS",
        (Comments, Tr) => "YORUMLAR",
        (TabMail, En) => "Mail",
        (TabMail, Tr) => "Posta",
        (WriteAComment, En) => "Write a comment…",
        (WriteAComment, Tr) => "Bir yorum yaz…",
        (Comment, En) => "Comment",
        (Comment, Tr) => "Yorum yap",
        (Notifications, En) => "NOTIFICATIONS",
        (Notifications, Tr) => "BİLDİRİMLER",
        (Retry, En) => "Retry",
        (Retry, Tr) => "Yeniden dene",
        (Activity, En) => "ACTIVITY",
        (Activity, Tr) => "ETKİNLİK",
        (DeleteTask, En) => "Delete task",
        (DeleteTask, Tr) => "Görevi sil",
        (CommentGoesWithIt, En) => "1 comment goes with it",
        (CommentGoesWithIt, Tr) => "1 yorum onunla gider",
        (CommentsGoWithIt, En) => "comments go with it",
        (CommentsGoWithIt, Tr) => "yorum onunla gider",
        (DependencyStopsApplying, En) => "1 dependency stops applying",
        (DependencyStopsApplying, Tr) => "1 bağımlılık geçerliliğini yitirir",
        (DependenciesStopApplying, En) => "dependencies stop applying",
        (DependenciesStopApplying, Tr) => "bağımlılık geçerliliğini yitirir",
        (StopsBeingBlocked, En) => "stops being blocked",
        (StopsBeingBlocked, Tr) => "artık engellenmiyor",
        (ThisTaskCannotBeDeleted, En) => "This task cannot be deleted.",
        (ThisTaskCannotBeDeleted, Tr) => "Bu görev silinemez.",
        (Close, En) => "Close",
        (Close, Tr) => "Kapat",
        (Blocks, En) => "blocks",
        (Blocks, Tr) => "engelliyor",
        (BlockedBy, En) => "blocked by",
        (BlockedBy, Tr) => "engelleyen",
        (Overdue, En) => "overdue",
        (Overdue, Tr) => "gecikmiş",
        (Blocked, En) => "blocked",
        (Blocked, Tr) => "engelli",

        (NavBoard, En) => "Board",
        (NavBoard, Tr) => "Pano",
        (NavMailRules, En) => "Mail rules",
        (NavMailRules, Tr) => "Posta kuralları",
        (NavLogs, En) => "Logs",
        (NavLogs, Tr) => "Kayıtlar",
        (NavSettings, En) => "Settings",
        (NavSettings, Tr) => "Ayarlar",
        (AdminOnly, En) => "Admin only",
        (AdminOnly, Tr) => "Sadece yönetici",

        (NavTags, En) => "Tags",
        (NavTags, Tr) => "Etiketler",
        (NewTag, En) => "New tag",
        (NewTag, Tr) => "Yeni etiket",
        (NameLabel, En) => "NAME",
        (NameLabel, Tr) => "İSİM",
        (AddTag, En) => "Add tag",
        (AddTag, Tr) => "Etiket ekle",
        (SaveTag, En) => "Save tag",
        (SaveTag, Tr) => "Etiketi kaydet",
        (EditThisTag, En) => "Edit this tag",
        (EditThisTag, Tr) => "Bu etiketi düzenle",
        (DeleteThisTag, En) => "Delete this tag",
        (DeleteThisTag, Tr) => "Bu etiketi sil",
        (MoveTagUp, En) => "Move up",
        (MoveTagUp, Tr) => "Yukarı taşı",
        (MoveTagDown, En) => "Move down",
        (MoveTagDown, Tr) => "Aşağı taşı",
        (Project, En) => "Tag",
        (Project, Tr) => "Etiket",
        (AllTags, En) => "All",
        (AllTags, Tr) => "Tümü",

        (YourProfile, En) => "Your profile",
        (YourProfile, Tr) => "Profilin",
        (Remove, En) => "Remove",
        (Remove, Tr) => "Kaldır",
        (DisplayNameLabel, En) => "DISPLAY NAME",
        (DisplayNameLabel, Tr) => "GÖRÜNEN AD",
        (EmailLabel, En) => "EMAIL",
        (EmailLabel, Tr) => "E-POSTA",
        (TimezoneLabel, En) => "TIMEZONE",
        (TimezoneLabel, Tr) => "SAAT DİLİMİ",
        (ThemeLabel, En) => "THEME",
        (ThemeLabel, Tr) => "TEMA",
        (UiLabel, En) => "INTERFACE",
        (UiLabel, Tr) => "ARAYÜZ",
        (LightOption, En) => "Light",
        (LightOption, Tr) => "Açık",
        (DarkOption, En) => "Dark",
        (DarkOption, Tr) => "Koyu",
        (LanguageLabel, En) => "LANGUAGE",
        (LanguageLabel, Tr) => "DİL",
        (Saved, En) => "Saved.",
        (Saved, Tr) => "Kaydedildi.",
        (SignOut, En) => "Sign out",
        (SignOut, Tr) => "Oturumu kapat",
        (OutgoingMail, En) => "Outgoing mail",
        (OutgoingMail, Tr) => "Giden posta",
        (Connected, En) => "Connected",
        (Connected, Tr) => "Bağlı",
        (NotConfiguredChip, En) => "Not configured",
        (NotConfiguredChip, Tr) => "Yapılandırılmadı",
        (Unchecked, En) => "Unchecked",
        (Unchecked, Tr) => "Denenmedi",
        (Refused, En) => "Refused",
        (Refused, Tr) => "Reddedildi",
        (CheckConnection, En) => "Check connection",
        (CheckConnection, Tr) => "Bağlantıyı dene",
        (NotConnectedNote, En) => "Not connected.",
        (NotConnectedNote, Tr) => "Bağlı değil.",
        (SmtpHostLabel, En) => "SMTP HOST",
        (SmtpHostLabel, Tr) => "SMTP SUNUCUSU",
        (PortLabel, En) => "PORT",
        (PortLabel, Tr) => "PORT",
        (UsernameLabel, En) => "USERNAME",
        (UsernameLabel, Tr) => "KULLANICI ADI",
        (PasswordLabel, En) => "PASSWORD",
        (PasswordLabel, Tr) => "PAROLA",
        (PasswordSetPlaceholder, En) => "Unchanged",
        (PasswordSetPlaceholder, Tr) => "Değişmedi",
        (PasswordNeededPlaceholder, En) => "Required",
        (PasswordNeededPlaceholder, Tr) => "Gerekli",
        (FromNameLabel, En) => "FROM NAME",
        (FromNameLabel, Tr) => "GÖNDEREN ADI",
        (FromAddressLabel, En) => "FROM ADDRESS",
        (FromAddressLabel, Tr) => "GÖNDEREN ADRESİ",
        (SendTestMail, En) => "Send test mail to myself",
        (SendTestMail, Tr) => "Kendime test postası gönder",
        (WorkspaceLimits, En) => "Workspace limits",
        (WorkspaceLimits, Tr) => "Çalışma alanı limitleri",
        (AttachmentLimitLabel, En) => "ATTACHMENT LIMIT (MB)",
        (AttachmentLimitLabel, Tr) => "EK LİMİTİ (MB)",
        (PhotoLimitLabel, En) => "PHOTO LIMIT (MB)",
        (PhotoLimitLabel, Tr) => "FOTOĞRAF LİMİTİ (MB)",
        (AllowedFileTypesLabel, En) => "ALLOWED FILE TYPES",
        (AllowedFileTypesLabel, Tr) => "İZİN VERİLEN DOSYA TÜRLERİ",
        (Members, En) => "Members",
        (Members, Tr) => "Üyeler",
        (Message, En) => "Message",
        (Message, Tr) => "Mesaj",
        (Everyone, En) => "Everyone",
        (Everyone, Tr) => "Herkes",
        (Recipient, En) => "RECIPIENT",
        (Recipient, Tr) => "ALICI",
        (Subject, En) => "SUBJECT",
        (Subject, Tr) => "KONU",
        (Body, En) => "BODY",
        (Body, Tr) => "İÇERİK",
        (Send, En) => "Send",
        (Send, Tr) => "Gönder",
        (NameCol, En) => "NAME",
        (NameCol, Tr) => "AD",
        (AddressCol, En) => "ADDRESS",
        (AddressCol, Tr) => "ADRES",
        (RoleCol, En) => "ROLE",
        (RoleCol, Tr) => "ROL",
        (AccountCol, En) => "ACCOUNT",
        (AccountCol, Tr) => "HESAP",
        (OwnerStatus, En) => "owner",
        (OwnerStatus, Tr) => "kurucu",
        (InvitedStatus, En) => "invited",
        (InvitedStatus, Tr) => "davet edildi",
        (ActiveStatus, En) => "active",
        (ActiveStatus, Tr) => "aktif",
        (You, En) => "you",
        (You, Tr) => "sen",
        (ResendMail, En) => "Resend mail",
        (ResendMail, Tr) => "Postayı yeniden gönder",
        (SendSigninLink, En) => "Send a sign-in link",
        (SendSigninLink, Tr) => "Giriş bağlantısı gönder",
        (RoleMemberOption, En) => "Member",
        (RoleMemberOption, Tr) => "Üye",
        (RoleViewerOption, En) => "Viewer",
        (RoleViewerOption, Tr) => "İzleyici",
        (RoleAdminOption, En) => "Admin",
        (RoleAdminOption, Tr) => "Yönetici",
        (AddMember, En) => "Add member",
        (AddMember, Tr) => "Üye ekle",
        (SmtpHostRequired, En) => "Give the SMTP host.",
        (SmtpHostRequired, Tr) => "SMTP sunucusunu ver.",
        (SmtpHostInvalid, En) => "The SMTP host is a host name, not an address or a URL.",
        (SmtpHostInvalid, Tr) => "SMTP sunucusu bir sunucu adıdır, adres ya da URL değil.",
        (PortInvalid, En) => "A port is a number between 1 and 65535.",
        (PortInvalid, Tr) => "Port, 1 ile 65535 arasında bir sayıdır.",
        (SmtpUsernameRequired, En) => "Give the SMTP username.",
        (SmtpUsernameRequired, Tr) => "SMTP kullanıcı adını ver.",
        (PasswordNeededFirstTime, En) => "A password is needed the first time.",
        (PasswordNeededFirstTime, Tr) => "İlk seferde bir parola gerekir.",
        (NotFromAddress, En) => "That is not a from-address.",
        (NotFromAddress, Tr) => "Bu bir gönderen adresi değil.",
        (NotAnOrigin, En) => "Not an http:// or https:// address.",
        (NotAnOrigin, Tr) => "http:// veya https:// adresi değil.",
        (LinkAddressLabel, En) => "Link address",
        (LinkAddressLabel, Tr) => "Bağlantı adresi",
        (SenderNotConfiguredYet, En) => "No sender to test.",
        (SenderNotConfiguredYet, Tr) => "Test edilecek gönderen yok.",

        (LinkExpiredTitle, En) => "This link no longer works",
        (LinkExpiredTitle, Tr) => "Bu bağlantı artık çalışmıyor",
        (SetupTitle, En) => "Set up İz",
        (SetupTitle, Tr) => "İz'i kur",
        (SetupSub, En) => "First account becomes the admin.",
        (SetupSub, Tr) => "İlk hesap yönetici olur.",
        (YourNameLabel, En) => "YOUR NAME",
        (YourNameLabel, Tr) => "ADIN",
        (CreateWorkspace, En) => "Create workspace",
        (CreateWorkspace, Tr) => "Çalışma alanı oluştur",
        (SignInTitle, En) => "Sign in to İz",
        (SignInTitle, Tr) => "İz'e giriş yap",
        (SignInSub, En) => {
            "Accounts are made by the admin. If you were invited, use the link you were sent — it is where you choose your password."
        }
        (SignInSub, Tr) => {
            "Hesaplar yönetici tarafından oluşturulur. Davet edildiysen, sana gönderilen bağlantıyı kullan — parolanı orada seçersin."
        }
        (SignInButton, En) => "Sign in",
        (SignInButton, Tr) => "Giriş yap",
        (PickPasswordTitle, En) => "Pick a password",
        (PickPasswordTitle, Tr) => "Bir parola seç",
        (SigningInAsLabel, En) => "SIGNING IN AS",
        (SigningInAsLabel, Tr) => "GİRİŞ YAPILAN HESAP",
        (NewPasswordLabel, En) => "NEW PASSWORD",
        (NewPasswordLabel, Tr) => "YENİ PAROLA",
        (SetPasswordAndSignIn, En) => "Set password and sign in",
        (SetPasswordAndSignIn, Tr) => "Parolayı ayarla ve giriş yap",
        (CurrentPasswordLabel, En) => "CURRENT PASSWORD",
        (CurrentPasswordLabel, Tr) => "MEVCUT PAROLA",
        (ChangePassword, En) => "Change password",
        (ChangePassword, Tr) => "Parolayı değiştir",
        (AdminMadeYouAnAccount, En) => "An admin made you an account.",
        (AdminMadeYouAnAccount, Tr) => "Bir yönetici sana hesap oluşturdu.",

        (MailRules, En) => "Mail rules",
        (MailRules, Tr) => "Posta kuralları",
        (EditLabel, En) => "Edit",
        (EditLabel, Tr) => "Düzenle",
        (NoSenderConnectedPrefix, En) => "No sender connected.",
        (NoSenderConnectedPrefix, Tr) => "Bağlı gönderen yok.",
        (NewRule, En) => "New rule",
        (NewRule, Tr) => "Yeni kural",
        (WhenLabel, En) => "WHEN",
        (WhenLabel, Tr) => "NE ZAMAN",
        (ColumnLabel, En) => "COLUMN",
        (ColumnLabel, Tr) => "SÜTUN",
        (SendLabel, En) => "SEND",
        (SendLabel, Tr) => "GÖNDER",
        (ToLabel, En) => "TO",
        (ToLabel, Tr) => "KİME",
        (BodyLabel, En) => "BODY",
        (BodyLabel, Tr) => "İÇERİK",
        (TaskDetails, En) => "Task details",
        (TaskDetails, Tr) => "Görev ayrıntıları",
        (AddRule, En) => "Add rule",
        (AddRule, Tr) => "Kural ekle",
        (SaveRule, En) => "Save rule",
        (SaveRule, Tr) => "Kuralı kaydet",
        (EditThisRule, En) => "Edit this rule",
        (EditThisRule, Tr) => "Bu kuralı düzenle",
        (DeleteThisRule, En) => "Delete this rule",
        (DeleteThisRule, Tr) => "Bu kuralı sil",
        (SwitchRuleOff, En) => "Switch this rule off",
        (SwitchRuleOff, Tr) => "Bu kuralı kapat",
        (SwitchRuleOn, En) => "Switch this rule on",
        (SwitchRuleOn, Tr) => "Bu kuralı aç",
        (NeverFired, En) => "never fired",
        (NeverFired, Tr) => "hiç tetiklenmedi",
        (NoRulesYet, En) => "No rules yet.",
        (NoRulesYet, Tr) => "Henüz kural yok.",
        (TriggerStatusBecomes, En) => "status becomes",
        (TriggerStatusBecomes, Tr) => "durum şuna dönüşür",
        (TriggerUnblocked, En) => "a task stops being blocked",
        (TriggerUnblocked, Tr) => "bir görev engellenmekten çıkar",
        (TriggerCreated, En) => "a task is created",
        (TriggerCreated, Tr) => "bir görev oluşturulur",
        (TriggerAssigned, En) => "someone is assigned",
        (TriggerAssigned, Tr) => "biri atanır",
        (TriggerUnassigned, En) => "someone is unassigned",
        (TriggerUnassigned, Tr) => "birinin ataması kaldırılır",
        (TriggerCommented, En) => "a comment is written",
        (TriggerCommented, Tr) => "bir yorum yazılır",
        (TriggerDeadlineSet, En) => "a deadline is set",
        (TriggerDeadlineSet, Tr) => "bir son tarih belirlenir",
        (TriggerDeadlineCleared, En) => "a deadline is removed",
        (TriggerDeadlineCleared, Tr) => "bir son tarih kaldırılır",
        (TriggerRetitled, En) => "a task is renamed",
        (TriggerRetitled, Tr) => "bir görev yeniden adlandırılır",
        (TriggerLinked, En) => "a task is linked",
        (TriggerLinked, Tr) => "bir görev bağlanır",
        (TriggerUnlinked, En) => "a task is unlinked",
        (TriggerUnlinked, Tr) => "bir görevin bağlantısı kaldırılır",
        (TriggerDeleted, En) => "a task is deleted",
        (TriggerDeleted, Tr) => "bir görev silinir",
        (AudienceAssignees, En) => "assignees",
        (AudienceAssignees, Tr) => "atananlar",
        (AudienceBoard, En) => "everyone on board",
        (AudienceBoard, Tr) => "pano üzerindeki herkes",
        (AudienceCreator, En) => "its creator",
        (AudienceCreator, Tr) => "oluşturanı",
        (WhenStatusBecomes, En) => "When status becomes",
        (WhenStatusBecomes, Tr) => "Durum şuna dönüştüğünde",
        (WhenStatusChanges, En) => "When status changes",
        (WhenStatusChanges, Tr) => "Durum değiştiğinde",
        (AnyColumn, En) => "Any column",
        (AnyColumn, Tr) => "Herhangi bir sütun",
        (WhenTaskStopsBeingBlocked, En) => "When a task stops being",
        (WhenTaskStopsBeingBlocked, Tr) => "Bir görev şu olmaktan çıktığında",
        (WhenTaskCreated, En) => "When a task is created",
        (WhenTaskCreated, Tr) => "Bir görev oluşturulduğunda",
        (WhenSomeoneAssigned, En) => "When someone is assigned",
        (WhenSomeoneAssigned, Tr) => "Biri atandığında",
        (WhenSomeoneUnassigned, En) => "When someone is unassigned",
        (WhenSomeoneUnassigned, Tr) => "Birinin ataması kaldırıldığında",
        (WhenCommentWritten, En) => "When a comment is written",
        (WhenCommentWritten, Tr) => "Bir yorum yazıldığında",
        (WhenDeadlineSet, En) => "When a deadline is set",
        (WhenDeadlineSet, Tr) => "Bir son tarih belirlendiğinde",
        (WhenDeadlineRemoved, En) => "When a deadline is removed",
        (WhenDeadlineRemoved, Tr) => "Bir son tarih kaldırıldığında",
        (WhenTaskRenamed, En) => "When a task is renamed",
        (WhenTaskRenamed, Tr) => "Bir görev yeniden adlandırıldığında",
        (WhenTaskLinked, En) => "When a task is linked",
        (WhenTaskLinked, Tr) => "Bir görev bağlandığında",
        (WhenTaskUnlinked, En) => "When a task is unlinked",
        (WhenTaskUnlinked, Tr) => "Bir görevin bağlantısı kaldırıldığında",
        (WhenTaskDeleted, En) => "When a task is deleted",
        (WhenTaskDeleted, Tr) => "Bir görev silindiğinde",
        (BlockedWord, En) => "blocked",
        (BlockedWord, Tr) => "engelli",
        (ColumnGone, En) => "a column that is gone",
        (ColumnGone, Tr) => "artık olmayan bir sütun",
        (AudAssignees, En) => "assignees",
        (AudAssignees, Tr) => "atananlar",
        (AudEveryoneOnBoard, En) => "everyone on board",
        (AudEveryoneOnBoard, Tr) => "pano üzerindeki herkes",
        (AudItsCreator, En) => "its creator",
        (AudItsCreator, Tr) => "oluşturanı",
        (SendConnector, En) => "send",
        (SendConnector, Tr) => "gönder",
        (ToConnector, En) => "to",
        (ToConnector, Tr) => "kime",

        (Logs, En) => "Logs",
        (Logs, Tr) => "Kayıtlar",
        (MailQueue, En) => "Mail queue",
        (MailQueue, Tr) => "Posta kuyruğu",
        (MailDecisions, En) => "Mail decisions",
        (MailDecisions, Tr) => "Posta kararları",
        (WorkspaceActivity, En) => "Workspace activity",
        (WorkspaceActivity, Tr) => "Çalışma alanı etkinliği",
        (NothingOwed, En) => "Nothing owed.",
        (NothingOwed, Tr) => "Bekleyen yok.",
        (NoDecisionsYet, En) => "No decisions yet.",
        (NoDecisionsYet, Tr) => "Henüz karar yok.",
        (NothingYet, En) => "Nothing yet.",
        (NothingYet, Tr) => "Henüz bir şey yok.",
        (NoRetry, En) => "no retry",
        (NoRetry, Tr) => "yeniden denenmeyecek",
        (NextTry, En) => "next try",
        (NextTry, Tr) => "sonraki deneme",
        (DueNow, En) => "due",
        (DueNow, Tr) => "sırada",
        (Older, En) => "Older",
        (Older, Tr) => "Eski",
        (Newer, En) => "Newer",
        (Newer, Tr) => "Yeni",
        (QueueStatePending, En) => "pending",
        (QueueStatePending, Tr) => "bekliyor",
        (QueueStateHeld, En) => "held",
        (QueueStateHeld, Tr) => "tutuluyor",
        (QueueStateFailed, En) => "failed",
        (QueueStateFailed, Tr) => "başarısız",
        (QueueStateSent, En) => "sent",
        (QueueStateSent, Tr) => "gönderildi",
        (QueueStateAbandoned, En) => "abandoned",
        (QueueStateAbandoned, Tr) => "vazgeçildi",
        (RuleGone, En) => "a rule that is gone",
        (RuleGone, Tr) => "artık olmayan bir kural",
        (TaskGoneLabel, En) => "a task that is gone",
        (TaskGoneLabel, Tr) => "artık olmayan bir görev",
        (OutcomeQueued, En) => "queued",
        (OutcomeQueued, Tr) => "kuyruğa alındı",
        (OutcomeAlreadyQueued, En) => "already queued",
        (OutcomeAlreadyQueued, Tr) => "zaten kuyrukta",
        (OutcomeNoRecipients, En) => "nobody to mail",
        (OutcomeNoRecipients, Tr) => "postalanacak kimse yok",
        (OutcomeNotMatched, En) => "did not match",
        (OutcomeNotMatched, Tr) => "eşleşmedi",
        (OutcomeRuleOff, En) => "rule off",
        (OutcomeRuleOff, Tr) => "kural kapalı",
        (OutcomeTaskGone, En) => "task gone",
        (OutcomeTaskGone, Tr) => "görev yok oldu",
        (ActCreated, En) => "created this task",
        (ActCreated, Tr) => "bu görevi oluşturdu",
        (ActRetitled, En) => "renamed this task",
        (ActRetitled, Tr) => "bu görevi yeniden adlandırdı",
        (ActDescribed, En) => "edited the description",
        (ActDescribed, Tr) => "açıklamayı düzenledi",
        (ActDeadlineCleared, En) => "removed the deadline",
        (ActDeadlineCleared, Tr) => "son tarihi kaldırdı",
        (ActClockCleared, En) => "removed the time",
        (ActClockCleared, Tr) => "saati kaldırdı",
        (ActDeleted, En) => "deleted this task",
        (ActDeleted, Tr) => "bu görevi sildi",
        (ActCommented, En) => "commented",
        (ActCommented, Tr) => "yorum yaptı",
        (ActWorkspaceClaimed, En) => "claimed the workspace",
        (ActWorkspaceClaimed, Tr) => "çalışma alanını kurdu",
        (ActJoined, En) => "joined",
        (ActJoined, Tr) => "katıldı",
        (ActSignedIn, En) => "signed in",
        (ActSignedIn, Tr) => "oturum açtı",
        (ActSignedOut, En) => "signed out",
        (ActSignedOut, Tr) => "oturumu kapattı",
        (ActPasswordChanged, En) => "changed the password",
        (ActPasswordChanged, Tr) => "parolasını değiştirdi",
        (ActProfileSaved, En) => "saved the profile",
        (ActProfileSaved, Tr) => "profilini kaydetti",
        (ActSenderSaved, En) => "saved the sender settings",
        (ActSenderSaved, Tr) => "gönderen ayarlarını kaydetti",
        (ActLimitsSaved, En) => "saved the limits",
        (ActLimitsSaved, Tr) => "sınırları kaydetti",
        (ActTestMailSent, En) => "sent a test mail",
        (ActTestMailSent, Tr) => "test postası gönderdi",
        (ActMessageSent, En) => "sent a message",
        (ActMessageSent, Tr) => "mesaj gönderdi",
        (ActSendRetried, En) => "retried a send",
        (ActSendRetried, Tr) => "bir gönderimi yeniden denedi",
        (ActPhotoSaved, En) => "saved the profile photo",
        (ActPhotoSaved, Tr) => "profil fotoğrafını kaydetti",
        (ActPhotoRemoved, En) => "removed the profile photo",
        (ActPhotoRemoved, Tr) => "profil fotoğrafını kaldırdı",
        (ActSenderChecked, En) => "checked the mail server",
        (ActSenderChecked, Tr) => "posta sunucusunu denetledi",
        (PWCurrentWrong, En) => "The current password is wrong.",
        (PWCurrentWrong, Tr) => "Mevcut parola yanlış.",
        (PWIsCurrent, En) => "That's your current password.",
        (PWIsCurrent, Tr) => "Bu zaten mevcut parolan.",
        (PWTooShort, En) => "At least 10 characters.",
        (PWTooShort, Tr) => "En az 10 karakter.",
        (PWLooksLikeYou, En) => "Not your address or your name.",
        (PWLooksLikeYou, Tr) => "Adresin ya da adın değil.",
        (PasswordSaved, En) => "Password changed. Your other devices were signed out.",
        (PasswordSaved, Tr) => "Parola değişti. Diğer cihazlarının oturumu kapatıldı.",
        (UnblockedWord, En) => "unblocked",
        (UnblockedWord, Tr) => "engeli kaldırıldı",
        (AColumn, En) => "a column",
        (AColumn, Tr) => "bir sütun",
        (AudienceEmpty, En) => "audience is empty",
        (AudienceEmpty, Tr) => "kitle boş",
        (AudienceActorOnly, En) => "audience was only the actor",
        (AudienceActorOnly, Tr) => "kitle sadece işlemi yapan kişiydi",
        (NotAStatusCrossing, En) => "not a status crossing",
        (NotAStatusCrossing, Tr) => "bir durum geçişi değil",
        (NotAnUnblockedEvent, En) => "not an unblocked event",
        (NotAnUnblockedEvent, Tr) => "engel kalkma olayı değil",
        (TheSystem, En) => "The system",
        (TheSystem, Tr) => "Sistem",
        (NoDeadline, En) => "no deadline",
        (NoDeadline, Tr) => "tarih yok",
        (NoTasks, En) => "No tasks",
        (NoTasks, Tr) => "Görev yok",
        (NoDescription, En) => "no description",
        (NoDescription, Tr) => "açıklama yok",
        (NoDependencies, En) => "no dependencies",
        (NoDependencies, Tr) => "bağımlılık yok",
        (SubtaskGoesWithIt, En) => "its subtask goes with it",
        (SubtaskGoesWithIt, Tr) => "alt görevi de silinir",
        (SubtasksGoWithIt, En) => "subtasks go with it",
        (SubtasksGoWithIt, Tr) => "alt görevi de silinir",
        (Subtasks, En) => "SUBTASKS",
        (Subtasks, Tr) => "ALT GÖREVLER",
        (NoSubtasks, En) => "no subtasks",
        (NoSubtasks, Tr) => "alt görev yok",
        (NewSubtask, En) => "Subtask",
        (NewSubtask, Tr) => "Alt görev",
        (AddSubtask, En) => "Add",
        (AddSubtask, Tr) => "Ekle",
        (MakeAPart, En) => "Take an existing task",
        (MakeAPart, Tr) => "Var olan görevi al",
        (ReleaseThisPart, En) => "Release this subtask",
        (ReleaseThisPart, Tr) => "Bu alt görevi serbest bırak",
        (ExistingTask, En) => "Take",
        (ExistingTask, Tr) => "Al",
        (DonePrefix, En) => "done ",
        (DonePrefix, Tr) => "bitti ",
        (Clear, En) => "Clear",
        (Clear, Tr) => "Temizle",
        (Today, En) => "Today",
        (Today, Tr) => "Bugün",
        (PreviousMonth, En) => "Previous month",
        (PreviousMonth, Tr) => "Önceki ay",
        (NextMonth, En) => "Next month",
        (NextMonth, Tr) => "Sonraki ay",
        (WeekdayInitials, En) => "M,T,W,T,F,S,S",
        (WeekdayInitials, Tr) => "P,S,Ç,P,C,C,P",
        (MonthJanuary, En) => "January",
        (MonthJanuary, Tr) => "Ocak",
        (MonthFebruary, En) => "February",
        (MonthFebruary, Tr) => "Şubat",
        (MonthMarch, En) => "March",
        (MonthMarch, Tr) => "Mart",
        (MonthApril, En) => "April",
        (MonthApril, Tr) => "Nisan",
        (MonthMay, En) => "May",
        (MonthMay, Tr) => "Mayıs",
        (MonthJune, En) => "June",
        (MonthJune, Tr) => "Haziran",
        (MonthJuly, En) => "July",
        (MonthJuly, Tr) => "Temmuz",
        (MonthAugust, En) => "August",
        (MonthAugust, Tr) => "Ağustos",
        (MonthSeptember, En) => "September",
        (MonthSeptember, Tr) => "Eylül",
        (MonthOctober, En) => "October",
        (MonthOctober, Tr) => "Ekim",
        (MonthNovember, En) => "November",
        (MonthNovember, Tr) => "Kasım",
        (MonthDecember, En) => "December",
        (MonthDecember, Tr) => "Aralık",
        (All, En) => "All",
        (All, Tr) => "Tümü",
        (LogSystem, En) => "System",
        (LogSystem, Tr) => "Sistem",
        (Newest, En) => "Newest",
        (Newest, Tr) => "En yeni",
        (Oldest, En) => "Oldest",
        (Oldest, Tr) => "En eski",
        (From, En) => "From",
        (From, Tr) => "Başlangıç",
        (To, En) => "To",
        (To, Tr) => "Bitiş",
        (TasksOpenLabel, En) => "Open tasks",
        (TasksOpenLabel, Tr) => "Açık görevler",
        (TasksDoneLabel, En) => "Done tasks",
        (TasksDoneLabel, Tr) => "Biten görevler",
        (TasksCreatedLabel, En) => "Tasks created",
        (TasksCreatedLabel, Tr) => "Oluşturulan görevler",
        (CommentsLabel, En) => "Comments",
        (CommentsLabel, Tr) => "Yorumlar",
        (RecentActivity, En) => "Recent activity",
        (RecentActivity, Tr) => "Son etkinlik",
        (JoinedLabel, En) => "JOINED",
        (JoinedLabel, Tr) => "KATILDI",
        (LastSeenLabel, En) => "LAST SEEN",
        (LastSeenLabel, Tr) => "SON GÖRÜLME",
        (InvitedByLabel, En) => "INVITED BY",
        (InvitedByLabel, Tr) => "DAVET EDEN",
        (MailBatchLabel, En) => "Mail delay (minutes)",
        (MailBatchLabel, Tr) => "Posta gecikmesi (dakika)",
        (ReminderMinutesLabel, En) => "Reminder (minutes)",
        (ReminderMinutesLabel, Tr) => "Hatırlatma (dakika)",
        (ClockHour, En) => "Hour",
        (ClockHour, Tr) => "Saat",
        (ClockMinute, En) => "Minute",
        (ClockMinute, Tr) => "Dakika",

        (Search, En) => "Search",
        (Search, Tr) => "Ara",
        (NoMatches, En) => "No matches",
        (NoMatches, Tr) => "Eşleşme yok",
        (ForgotIt, En) => "Forgot it?",
        (ForgotIt, Tr) => "Parolamı unuttum",
        (ForgotTitle, En) => "Reset the password",
        (ForgotTitle, Tr) => "Parolayı sıfırla",
        (ForgotSend, En) => "Send the link",
        (ForgotSend, Tr) => "Bağlantıyı gönder",
        (ForgotSent, En) => "If the address has an account, a reset link is on its way.",
        (ForgotSent, Tr) => "Adres bir hesaba aitse sıfırlama bağlantısı yolda.",
        (ResetTitle, En) => "Choose a new password",
        (ResetTitle, Tr) => "Yeni parola seç",
        (ResetLinkDead, En) => "This reset link no longer works",
        (ResetLinkDead, Tr) => "Bu sıfırlama bağlantısı artık çalışmıyor",
        (ActResetRequested, En) => "asked to reset the password",
        (ActResetRequested, Tr) => "parola sıfırlaması istedi",
        (ActResetDone, En) => "set a new password by reset link",
        (ActResetDone, Tr) => "sıfırlama bağlantısıyla yeni parola aldı",
        (ActReminded, En) => "sent a reminder mail",
        (ActReminded, Tr) => "hatırlatma gönderdi",
        (Assignee, En) => "Assignee",
        (Assignee, Tr) => "Atanan",
        (Nobody, En) => "Nobody",
        (Nobody, Tr) => "Kimse",
    }
}

/// The twelve month names, in a user's language, `January`-first — the order
/// [`js_month_names`] hands to the datepicker's inline script.
fn month_names(lang: Lang) -> [&'static str; 12] {
    use Key::*;
    [
        t(lang, MonthJanuary),
        t(lang, MonthFebruary),
        t(lang, MonthMarch),
        t(lang, MonthApril),
        t(lang, MonthMay),
        t(lang, MonthJune),
        t(lang, MonthJuly),
        t(lang, MonthAugust),
        t(lang, MonthSeptember),
        t(lang, MonthOctober),
        t(lang, MonthNovember),
        t(lang, MonthDecember),
    ]
}

/// The datepicker's month names and weekday initials as JS array/string
/// literals, embedded once per page in its inline script — the grid itself
/// is built in JS (see `detail.rs`'s `datepicker_script`).
pub fn datepicker_js_literals(lang: Lang) -> (String, String) {
    let months = month_names(lang)
        .map(|name| format!("\"{name}\""))
        .join(",");
    let weekdays = t(lang, Key::WeekdayInitials)
        .split(',')
        .map(|initial| format!("\"{initial}\""))
        .collect::<Vec<_>>()
        .join(",");
    (format!("[{months}]"), format!("[{weekdays}]"))
}

/// The queue's "attempt N" stamp, in a user's language.
pub fn attempt_label(lang: Lang, attempts: u32) -> String {
    match lang {
        Lang::En => format!("attempt {attempts}"),
        Lang::Tr => format!("{attempts}. deneme"),
    }
}

/// The member table's "last seen <day>" cell, in a user's language.
pub fn last_seen_label(lang: Lang, day: &str) -> String {
    match lang {
        Lang::En => format!("last seen {day}"),
        Lang::Tr => format!("son görülme {day}"),
    }
}

/// The invite panel's "Mailed to <address>" note, in a user's language.
pub fn mailed_to_label(lang: Lang, address: &str) -> String {
    match lang {
        Lang::En => format!("Mailed to {address}"),
        Lang::Tr => format!("{address} adresine postalandı"),
    }
}

/// The sender panel's failed-test note, in a user's language.
pub fn not_delivered_label(lang: Lang, moment: &str, problem: &str) -> String {
    match lang {
        Lang::En => format!("not delivered, {moment} — {problem}"),
        Lang::Tr => format!("iletilmedi, {moment} — {problem}"),
    }
}

/// The sender panel's successful-test note, in a user's language.
pub fn delivered_in_label(lang: Lang, took: &str, moment: &str) -> String {
    match lang {
        Lang::En => format!("delivered in {took} — {moment}"),
        Lang::Tr => format!("{took} içinde iletildi — {moment}"),
    }
}

/// A rule row's "last sent <moment>" stamp, in a user's language.
pub fn last_sent_label(lang: Lang, moment: &str) -> String {
    match lang {
        Lang::En => format!("last sent {moment}"),
        Lang::Tr => format!("son gönderim {moment}"),
    }
}

/// A rule row's "last fired <moment>" stamp, in a user's language.
pub fn last_fired_label(lang: Lang, moment: &str) -> String {
    match lang {
        Lang::En => format!("last fired {moment}"),
        Lang::Tr => format!("son tetiklenme {moment}"),
    }
}

/// The activity feed's per-kind sentence, for the kinds that carry a detail —
/// `logs.rs`'s `activity_sentence` and the detail screen's own equivalent
/// share this shape but not this function, since only `logs.rs` is
/// translated here.
pub fn deadline_set_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("set deadline {detail}"),
        Lang::Tr => format!("son tarihi {detail} olarak belirledi"),
    }
}

/// The clock's own sentence, mirroring `deadline_set_label` — the stored
/// RFC 3339 stamp rides in `detail` raw, exactly as the deadline's day does.
pub fn clock_set_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("set the time to {detail}"),
        Lang::Tr => format!("saati {detail} olarak belirledi"),
    }
}

pub fn assigned_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("assigned {detail}"),
        Lang::Tr => format!("{detail} atadı"),
    }
}

pub fn unassigned_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("unassigned {detail}"),
        Lang::Tr => format!("{detail} atamasını kaldırdı"),
    }
}

pub fn linked_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("linked {detail}"),
        Lang::Tr => format!("{detail} bağladı"),
    }
}

pub fn parented_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("made this a part of {detail}"),
        Lang::Tr => format!("bunu {detail} görevinin parçası yaptı"),
    }
}

pub fn unparented_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("released this from {detail}"),
        Lang::Tr => format!("bunu {detail} görevinden çıkardı"),
    }
}

pub fn unlinked_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("unlinked {detail}"),
        Lang::Tr => format!("{detail} bağlantısını kaldırdı"),
    }
}

pub fn moved_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("moved {detail}"),
        Lang::Tr => format!("{detail} taşıdı"),
    }
}

pub fn unblocked_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("unblocked this task — {detail}"),
        Lang::Tr => format!("bu görevin engelini kaldırdı — {detail}"),
    }
}

pub fn invited_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("invited {detail}"),
        Lang::Tr => format!("{detail} kişisine davet gönderdi"),
    }
}

pub fn link_resent_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("resent the link to {detail}"),
        Lang::Tr => format!("{detail} adresine bağlantıyı yeniden gönderdi"),
    }
}

pub fn sign_in_failed_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("failed to sign in as {detail}"),
        Lang::Tr => format!("{detail} olarak oturum açılamadı"),
    }
}

pub fn role_changed_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("made {detail}"),
        Lang::Tr => format!("{detail} yaptı"),
    }
}

pub fn rule_created_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("created rule {detail}"),
        Lang::Tr => format!("{detail} kuralını oluşturdu"),
    }
}

pub fn rule_edited_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("edited rule {detail}"),
        Lang::Tr => format!("{detail} kuralını düzenledi"),
    }
}

pub fn rule_toggled_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("toggled rule {detail}"),
        Lang::Tr => format!("{detail} kuralını açıp kapattı"),
    }
}

pub fn rule_deleted_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("deleted rule {detail}"),
        Lang::Tr => format!("{detail} kuralını sildi"),
    }
}

pub fn tagged_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("tagged it {detail}"),
        Lang::Tr => format!("{detail} ile etiketledi"),
    }
}

pub fn tag_created_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("created the tag {detail}"),
        Lang::Tr => format!("{detail} etiketini oluşturdu"),
    }
}

pub fn tag_renamed_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("renamed the tag {detail}"),
        Lang::Tr => format!("{detail} etiketini yeniden adlandırdı"),
    }
}

pub fn tag_deleted_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("deleted the tag {detail}"),
        Lang::Tr => format!("{detail} etiketini sildi"),
    }
}

pub fn tag_moved_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("moved the tag {detail}"),
        Lang::Tr => format!("{detail} etiketini taşıdı"),
    }
}

pub fn file_added_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("added {detail}"),
        Lang::Tr => format!("{detail} ekledi"),
    }
}

pub fn file_removed_label(lang: Lang, detail: &str) -> String {
    match lang {
        Lang::En => format!("removed {detail}"),
        Lang::Tr => format!("{detail} kaldırdı"),
    }
}

/// The decisions panel's "moved to <column>" note, in a user's language.
pub fn moved_to_label(lang: Lang, column: &str) -> String {
    match lang {
        Lang::En => format!("moved to {column}"),
        Lang::Tr => format!("{column} sütununa taşındı"),
    }
}

/// A crossing that did not match a `StatusBecomes` rule: it moved somewhere
/// the rule does not watch.
pub fn moved_not_watched_label(lang: Lang, to: &str, watched: &str) -> String {
    match lang {
        Lang::En => format!("moved to {to}, rule watches {watched}"),
        Lang::Tr => format!("{to} sütununa taşındı, kural {watched} sütununu izliyor"),
    }
}

/// A freed task whose freeing did not match a `StatusBecomes` rule.
pub fn freed_not_watched_label(lang: Lang, watched: &str) -> String {
    match lang {
        Lang::En => format!("freed a task, rule watches a move to {watched}"),
        Lang::Tr => {
            format!("bir görevin engelini kaldırdı, kural {watched} sütununa taşınmasını izliyor")
        }
    }
}

/// The short word for an activity kind, as it appears in a decision's detail
/// — not the full activity-strip sentence, just the bare event name.
pub fn activity_kind_word(lang: Lang, kind: &str) -> String {
    match (kind, lang) {
        ("created", Lang::En) => "created".to_string(),
        ("created", Lang::Tr) => "oluşturuldu".to_string(),
        ("retitled", Lang::En) => "retitled".to_string(),
        ("retitled", Lang::Tr) => "yeniden adlandırıldı".to_string(),
        ("described", Lang::En) => "described".to_string(),
        ("described", Lang::Tr) => "açıklandı".to_string(),
        ("deadline_set", Lang::En) => "deadline set".to_string(),
        ("deadline_set", Lang::Tr) => "son tarih belirlendi".to_string(),
        ("deadline_cleared", Lang::En) => "deadline cleared".to_string(),
        ("deadline_cleared", Lang::Tr) => "son tarih kaldırıldı".to_string(),
        ("clock_set", Lang::En) => "time set".to_string(),
        ("clock_set", Lang::Tr) => "saat belirlendi".to_string(),
        ("clock_cleared", Lang::En) => "time cleared".to_string(),
        ("clock_cleared", Lang::Tr) => "saat kaldırıldı".to_string(),
        ("assigned", Lang::En) => "assigned".to_string(),
        ("assigned", Lang::Tr) => "atandı".to_string(),
        ("unassigned", Lang::En) => "unassigned".to_string(),
        ("unassigned", Lang::Tr) => "atama kaldırıldı".to_string(),
        ("linked", Lang::En) => "linked".to_string(),
        ("linked", Lang::Tr) => "bağlandı".to_string(),
        ("unlinked", Lang::En) => "unlinked".to_string(),
        ("unlinked", Lang::Tr) => "bağlantı kaldırıldı".to_string(),
        ("moved", Lang::En) => "moved".to_string(),
        ("moved", Lang::Tr) => "taşındı".to_string(),
        ("unblocked", Lang::En) => "unblocked".to_string(),
        ("unblocked", Lang::Tr) => "engeli kaldırıldı".to_string(),
        ("deleted", Lang::En) => "deleted".to_string(),
        ("deleted", Lang::Tr) => "silindi".to_string(),
        ("commented", Lang::En) => "commented".to_string(),
        ("commented", Lang::Tr) => "yorum yapıldı".to_string(),
        ("tagged", Lang::En) => "tagged".to_string(),
        ("tagged", Lang::Tr) => "etiketlendi".to_string(),
        ("tag_created", Lang::En) => "tag created".to_string(),
        ("tag_created", Lang::Tr) => "etiket oluşturuldu".to_string(),
        ("tag_renamed", Lang::En) => "tag renamed".to_string(),
        ("tag_renamed", Lang::Tr) => "etiket yeniden adlandırıldı".to_string(),
        ("tag_deleted", Lang::En) => "tag deleted".to_string(),
        ("tag_deleted", Lang::Tr) => "etiket silindi".to_string(),
        ("tag_moved", Lang::En) => "tag moved".to_string(),
        ("tag_moved", Lang::Tr) => "etiket taşındı".to_string(),
        ("send_retried", Lang::En) => "send retried".to_string(),
        ("send_retried", Lang::Tr) => "gönderim yeniden denendi".to_string(),
        ("photo_saved", Lang::En) => "photo saved".to_string(),
        ("photo_saved", Lang::Tr) => "fotoğraf kaydedildi".to_string(),
        ("photo_removed", Lang::En) => "photo removed".to_string(),
        ("photo_removed", Lang::Tr) => "fotoğraf kaldırıldı".to_string(),
        ("sender_checked", Lang::En) => "mail server checked".to_string(),
        ("sender_checked", Lang::Tr) => "posta sunucusu denetlendi".to_string(),
        (other, _) => other.to_string(),
    }
}

/// An activity that did not match a rule: the activity's own word, and what
/// the rule watches instead.
pub fn happened_not_watched_label(lang: Lang, event_word: &str, watched: &str) -> String {
    match lang {
        Lang::En => format!("{event_word}, rule watches {watched}"),
        Lang::Tr => format!("{event_word}, kural {watched} izliyor"),
    }
}

/// The "what a rule watches" half of `happened_not_watched_label`, for a
/// rule watching a status crossing.
pub fn watches_move_phrase(lang: Lang, column: &str) -> String {
    match lang {
        Lang::En => format!("a move to {column}"),
        Lang::Tr => format!("{column} sütununa taşınmasını"),
    }
}

/// The "what a rule watches" half of `happened_not_watched_label`, for a
/// rule watching its last blocker clear.
pub fn watches_unblock_phrase(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "its last blocker clearing",
        Lang::Tr => "son engelinin kalkmasını",
    }
}

/// A board column's name, in the viewer's language. Column names are stored
/// data — the seeded defaults ship in English (`turso_store.rs`'s seed) and
/// mail rules match on the stored string — so translation happens at the
/// edge, like [`activity_kind_word`]: the four seeded names read Turkish,
/// and any other name (a renamed or custom column) passes through as data.
pub fn column_name(lang: Lang, name: &str) -> String {
    match (name, lang) {
        ("Backlog", Lang::Tr) => "Bekleyen".to_string(),
        ("In Progress", Lang::Tr) => "Devam Ediyor".to_string(),
        ("Review", Lang::Tr) => "İncelemede".to_string(),
        ("Done", Lang::Tr) => "Tamamlandı".to_string(),
        (other, _) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four seeded columns read Turkish for a Turkish viewer and stay
    /// English otherwise; a column that is not one of the seeds is data and
    /// passes through in every language.
    #[test]
    fn seeded_columns_translate() {
        for (stored, tr) in [
            ("Backlog", "Bekleyen"),
            ("In Progress", "Devam Ediyor"),
            ("Review", "İncelemede"),
            ("Done", "Tamamlandı"),
        ] {
            assert_eq!(column_name(Lang::Tr, stored), tr);
            assert_eq!(column_name(Lang::En, stored), stored);
        }
        assert_eq!(column_name(Lang::Tr, "Shipped"), "Shipped");
    }
}
