table! {
    balances (channel, user) {
        channel -> Text,
        user -> Text,
        amount -> Integer,
    }
}

table! {
    commands (channel, name) {
        channel -> Text,
        name -> Text,
        count -> Integer,
        text -> Text,
    }
}

table! {
    after_streams (id) {
        id -> Integer,
        channel -> Nullable<Text>,
        added_at -> Timestamp,
        user -> Text,
        text -> Text,
    }
}

table! {
    bad_words (word) {
        word -> Text,
        why -> Nullable<Text>,
    }
}

table! {
    counters (channel, name) {
        channel -> Text,
        name -> Text,
        count -> Integer,
        text -> Text,
    }
}

table! {
    songs (id) {
        id -> Integer,
        deleted -> Bool,
        track_id -> Text,
        added_at -> Timestamp,
        promoted_at -> Nullable<Timestamp>,
        promoted_by -> Nullable<Text>,
        user -> Nullable<Text>,
    }
}

table! {
    set_values (channel, kind, value) {
        channel -> Text,
        kind -> Text,
        value -> Text,
    }
}

table! {
    settings (key) {
        key -> Text,
        value -> Text,
    }
}

table! {
    aliases (channel, name) {
        channel -> Text,
        name -> Text,
        text -> Text,
    }
}

table! {
    promotions (channel, name) {
        channel -> Text,
        name -> Text,
        frequency -> Integer,
        promoted_at -> Nullable<Timestamp>,
        text -> Text,
    }
}
