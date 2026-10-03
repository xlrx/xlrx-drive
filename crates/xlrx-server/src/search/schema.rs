//! Layout of the search index: fields, analyzers, file categories.

use tantivy::Index;
use tantivy::schema::{
    FAST, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer,
    TextAnalyzer, Token, TokenFilter, TokenStream, Tokenizer,
};

/// Changing fields or analyzers needs a new number: the index is then rebuilt from the database.
pub const VERSION: u32 = 1;

pub const PLAIN: &str = "xlrx_plain";
pub const GERMAN: &str = "xlrx_de";
pub const ENGLISH: &str = "xlrx_en";
pub const PREFIX: &str = "xlrx_prefix";

/// Longer "words" are mostly hashes, base64 or garbage from broken extraction.
const MAX_WORD: usize = 40;
/// Prefixes of name words are indexed up to this length (search while typing).
pub const PREFIX_MAX: usize = 20;

#[derive(Clone, Copy, Debug)]
pub struct Fields {
    /// Node id (one document per live node, except root directories).
    pub id: tantivy::schema::Field,
    pub root: tantivy::schema::Field,
    /// All directories above the node, from the root directory down (rights, `in:`).
    pub anc: tantivy::schema::Field,
    pub cat: tantivy::schema::Field,
    /// Lower-case file extension.
    pub ext: tantivy::schema::Field,
    /// Modification time, seconds since the epoch.
    pub mtime: tantivy::schema::Field,
    pub name: tantivy::schema::Field,
    pub name_de: tantivy::schema::Field,
    pub name_en: tantivy::schema::Field,
    /// Word prefixes of the name.
    pub name_pre: tantivy::schema::Field,
    /// Text in a language without stemmer, or of unknown language.
    pub text: tantivy::schema::Field,
    pub text_de: tantivy::schema::Field,
    pub text_en: tantivy::schema::Field,
}

pub fn build() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let words = |analyzer: &str| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(analyzer)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
    };
    let prefixes = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(PREFIX)
            .set_index_option(IndexRecordOption::WithFreqs),
    );
    let f = Fields {
        id: b.add_u64_field("id", INDEXED | STORED | FAST),
        root: b.add_u64_field("root", INDEXED),
        anc: b.add_u64_field("anc", INDEXED),
        cat: b.add_u64_field("cat", INDEXED | FAST),
        ext: b.add_text_field("ext", STRING),
        mtime: b.add_i64_field("mtime", INDEXED | FAST),
        name: b.add_text_field("name", words(PLAIN)),
        name_de: b.add_text_field("name_de", words(GERMAN)),
        name_en: b.add_text_field("name_en", words(ENGLISH)),
        name_pre: b.add_text_field("name_pre", prefixes),
        text: b.add_text_field("text", words(PLAIN)),
        text_de: b.add_text_field("text_de", words(GERMAN)),
        text_en: b.add_text_field("text_en", words(ENGLISH)),
    };
    (b.build(), f)
}

pub fn register(index: &Index) {
    let m = index.tokenizers();
    m.register(PLAIN, plain());
    m.register(GERMAN, stemmed(Language::German));
    m.register(ENGLISH, stemmed(Language::English));
    m.register(
        PREFIX,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(MAX_WORD))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .filter(EdgeNgrams { max: PREFIX_MAX })
            .build(),
    );
}

/// Words, lower case, accents folded ("Café" → "cafe").
pub fn plain() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_WORD))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

/// Like [`plain`], reduced to the word stem first ("Rechnungen" → "rechnung", "Häuser" → "haus").
fn stemmed(lang: Language) -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(MAX_WORD))
        .filter(LowerCaser)
        .filter(Stemmer::new(lang))
        .filter(AsciiFoldingFilter)
        .build()
}

/// Emits every prefix of each word up to `max` characters ("rech", "rechn", … for "rechnung").
#[derive(Clone)]
struct EdgeNgrams {
    max: usize,
}

impl TokenFilter for EdgeNgrams {
    type Tokenizer<T: Tokenizer> = EdgeNgramFilter<T>;

    fn transform<T: Tokenizer>(self, inner: T) -> EdgeNgramFilter<T> {
        EdgeNgramFilter {
            inner,
            max: self.max,
        }
    }
}

#[derive(Clone)]
struct EdgeNgramFilter<T> {
    inner: T,
    max: usize,
}

impl<T: Tokenizer> Tokenizer for EdgeNgramFilter<T> {
    type TokenStream<'a> = EdgeNgramStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        EdgeNgramStream {
            tail: self.inner.token_stream(text),
            max: self.max,
            ends: Vec::new(),
            next: 0,
            token: Token::default(),
        }
    }
}

struct EdgeNgramStream<S> {
    tail: S,
    max: usize,
    /// Byte ends of the prefixes of the current word.
    ends: Vec<usize>,
    next: usize,
    token: Token,
}

impl<S: TokenStream> TokenStream for EdgeNgramStream<S> {
    fn advance(&mut self) -> bool {
        loop {
            if let Some(&end) = self.ends.get(self.next) {
                let word = self.tail.token();
                self.token.text.clear();
                self.token.text.push_str(&word.text[..end]);
                self.token.offset_from = word.offset_from;
                self.token.offset_to = word.offset_to;
                self.token.position = word.position;
                self.token.position_length = 1;
                self.next += 1;
                return true;
            }
            if !self.tail.advance() {
                return false;
            }
            let word = &self.tail.token().text;
            self.ends.clear();
            self.ends.extend(
                word.char_indices()
                    .map(|(i, c)| i + c.len_utf8())
                    .take(self.max),
            );
            self.next = 0;
        }
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// Kinds of files for the filter `typ:` and the counts next to the results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Folder,
    Pdf,
    Document,
    Spreadsheet,
    Presentation,
    Image,
    Video,
    Audio,
    Mail,
    Text,
    Archive,
    Other,
}

impl Category {
    pub const ALL: [Category; 12] = [
        Category::Folder,
        Category::Pdf,
        Category::Document,
        Category::Spreadsheet,
        Category::Presentation,
        Category::Image,
        Category::Video,
        Category::Audio,
        Category::Mail,
        Category::Text,
        Category::Archive,
        Category::Other,
    ];

    pub fn code(self) -> u64 {
        self as u64
    }

    pub fn from_code(code: u64) -> Option<Self> {
        Self::ALL.get(usize::try_from(code).ok()?).copied()
    }

    /// Key in the API and in `typ:` (German, as the people using it type it).
    pub fn key(self) -> &'static str {
        match self {
            Category::Folder => "ordner",
            Category::Pdf => "pdf",
            Category::Document => "dokument",
            Category::Spreadsheet => "tabelle",
            Category::Presentation => "praesentation",
            Category::Image => "bild",
            Category::Video => "video",
            Category::Audio => "audio",
            Category::Mail => "mail",
            Category::Text => "text",
            Category::Archive => "archiv",
            Category::Other => "sonstiges",
        }
    }

    /// The category a `typ:` value names (singular, plural, a few synonyms), if any.
    pub fn parse(value: &str) -> Option<Self> {
        let v = value.to_lowercase().replace('ä', "ae");
        Some(match v.as_str() {
            "ordner" => Category::Folder,
            "pdf" | "pdfs" => Category::Pdf,
            "dokument" | "dokumente" | "doc" | "docs" => Category::Document,
            "tabelle" | "tabellen" => Category::Spreadsheet,
            "praesentation" | "praesentationen" | "folien" => Category::Presentation,
            "bild" | "bilder" | "foto" | "fotos" => Category::Image,
            "video" | "videos" | "film" | "filme" => Category::Video,
            "audio" | "musik" => Category::Audio,
            "mail" | "mails" | "email" | "e-mail" | "e-mails" => Category::Mail,
            "text" | "texte" => Category::Text,
            "archiv" | "archive" => Category::Archive,
            "sonstiges" => Category::Other,
            _ => return None,
        })
    }

    pub fn of(is_dir: bool, ext: &str) -> Self {
        if is_dir {
            return Category::Folder;
        }
        match ext {
            "pdf" => Category::Pdf,
            "doc" | "docx" | "docm" | "dot" | "dotx" | "odt" | "ott" | "rtf" | "pages" | "wpd"
            | "wps" => Category::Document,
            "xls" | "xlsx" | "xlsm" | "xlsb" | "ods" | "csv" | "tsv" | "numbers" => {
                Category::Spreadsheet
            }
            "ppt" | "pptx" | "pps" | "ppsx" | "odp" | "key" => Category::Presentation,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "tif" | "tiff" | "bmp"
            | "svg" | "avif" | "raw" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2"
            | "psd" => Category::Image,
            "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "mpg" | "mpeg" | "3gp" | "mts"
            | "m2ts" | "wmv" => Category::Video,
            "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "oga" | "opus" | "aif" | "aiff"
            | "wma" => Category::Audio,
            "eml" | "msg" | "mbox" => Category::Mail,
            "txt" | "md" | "markdown" | "rst" | "log" | "json" | "xml" | "yaml" | "yml"
            | "toml" | "ini" | "conf" | "html" | "htm" | "css" | "js" | "ts" | "rs" | "py"
            | "swift" | "java" | "kt" | "c" | "h" | "cpp" | "hpp" | "go" | "rb" | "php" | "sh"
            | "sql" | "tex" | "vcf" | "ics" => Category::Text,
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "dmg" | "iso" => {
                Category::Archive
            }
            _ => Category::Other,
        }
    }
}

/// Lower-case extension of a file name ("" if none; a leading dot alone is no extension).
pub fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(a: &mut TextAnalyzer, text: &str) -> Vec<String> {
        let mut s = a.token_stream(text);
        let mut out = Vec::new();
        while s.advance() {
            out.push(s.token().text.clone());
        }
        out
    }

    #[test]
    fn german_words_meet_at_their_stem() {
        let mut de = stemmed(Language::German);
        assert_eq!(tokens(&mut de, "Rechnungen"), tokens(&mut de, "Rechnung"));
        assert_eq!(tokens(&mut de, "Häuser"), tokens(&mut de, "Haus"));
        assert_eq!(tokens(&mut de, "Straße"), tokens(&mut de, "strasse"));
    }

    #[test]
    fn prefixes_of_every_word() {
        let mut p = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .filter(EdgeNgrams { max: 4 })
            .build();
        assert_eq!(
            tokens(&mut p, "Über_Ab"),
            ["u", "ub", "ube", "uber", "a", "ab"]
        );
    }

    #[test]
    fn categories() {
        assert_eq!(
            Category::of(false, &extension("Bericht.PDF")),
            Category::Pdf
        );
        assert_eq!(Category::of(false, &extension(".bashrc")), Category::Other);
        assert_eq!(Category::of(true, "pdf"), Category::Folder);
        assert_eq!(
            Category::parse("Präsentationen"),
            Some(Category::Presentation)
        );
        for c in Category::ALL {
            assert_eq!(Category::from_code(c.code()), Some(c));
            assert_eq!(Category::parse(c.key()), Some(c));
        }
    }
}
