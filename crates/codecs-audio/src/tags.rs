/// The handful of text tags that every supported audio container can carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    pub composer: Option<String>,
    pub date: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
}

impl Tags {
    /// Build from Vorbis-comment style `(NAME, value)` pairs (Ogg Vorbis, Opus, FLAC).
    pub fn from_vorbis_comments<'a>(fields: impl IntoIterator<Item = (&'a str, &'a str)>) -> Tags {
        let mut t = Tags::default();
        for (k, v) in fields {
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k.to_ascii_uppercase().as_str() {
                "TITLE" => t.title = Some(v.into()),
                "ARTIST" => t.artist = Some(v.into()),
                "ALBUM" => t.album = Some(v.into()),
                "ALBUMARTIST" => t.album_artist = Some(v.into()),
                "GENRE" => t.genre = Some(v.into()),
                "COMMENT" | "DESCRIPTION" => t.comment = Some(v.into()),
                "COMPOSER" => t.composer = Some(v.into()),
                "DATE" => t.date = Some(v.into()),
                "TRACKNUMBER" => t.track = v.split('/').next().and_then(|n| n.trim().parse().ok()),
                "DISCNUMBER" => t.disc = v.split('/').next().and_then(|n| n.trim().parse().ok()),
                _ => {}
            }
        }
        t
    }

    pub fn is_empty(&self) -> bool {
        *self == Tags::default()
    }

    /// (key, value) pairs using Vorbis-comment field names.
    pub fn vorbis_fields(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        let mut text = |k, o: &Option<String>| {
            if let Some(s) = o {
                v.push((k, s.clone()));
            }
        };
        text("TITLE", &self.title);
        text("ARTIST", &self.artist);
        text("ALBUM", &self.album);
        text("ALBUMARTIST", &self.album_artist);
        text("GENRE", &self.genre);
        text("COMMENT", &self.comment);
        text("COMPOSER", &self.composer);
        text("DATE", &self.date);
        if let Some(n) = self.track {
            v.push(("TRACKNUMBER", n.to_string()));
        }
        if let Some(n) = self.disc {
            v.push(("DISCNUMBER", n.to_string()));
        }
        v
    }
}
