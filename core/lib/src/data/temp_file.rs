use std::io;
use std::path::{PathBuf, Path};

use crate::http::{ContentType, Status};
use crate::data::{FromData, Data, Capped, N, Limits};
use crate::form::{FromFormField, ValueField, DataField, error::Errors};
use crate::outcome::IntoOutcome;
use crate::request::Request;

use tokio::fs::{self, File};
use tempfile::{NamedTempFile, TempPath};
use either::Either;

/// A file in temporary storage, deleted when dropped unless persisted.
///
/// `TempFile` is a data and form field (both value and data fields) guard that
/// streams incoming data into file in a temporary location. The file is deleted
/// when the `TempFile` handle is dropped. The file can be persisted with
/// [`TempFile::persist_to()`].
///
/// # Hazards
///
/// Temporary files are cleaned by system file cleaners periodically. While an
/// attempt is made not to delete temporary files in use, _detection_ of when a
/// temporary file is being used is unreliable. As a result, a time-of-check to
/// time-of-use race condition from the creation of a `TempFile` to the
/// persistance of the `TempFile` may occur. Specifically, the following
/// sequence may occur:
///
/// 1. A `TempFile` is created at random path `foo`.
/// 2. The system cleaner removes the file at path `foo`.
/// 3. Another application creates a file at path `foo`.
/// 4. The `TempFile`, ostesnsibly at path, `foo`, is persisted unexpectedly
///    with contents different from those in step 1.
///
/// To safe-guard against this issue, you should ensure that your temporary file
/// cleaner, if any, does not delete files too eagerly.
///
/// # Configuration
///
/// * **temporary file directory**
///
///   Configured via the [`temp_dir`](crate::Config::temp_dir) configuration
///   parameter, defaulting to the system's default temporary
///   ([`std::env::temp_dir()`]). Specifies where the files are stored.
///
/// * **data limit**
///
///   Controlled via [limits](crate::data::Limits) named `file` and `file/$ext`.
///   When used as a form guard, the extension `ext` is identified by the form
///   field's `Content-Type` ([`ContentType::extension()`]). When used as a data
///   guard, the extension is identified by the Content-Type of the request, if
///   any. If there is no Content-Type, the limit `file` is used.
///
/// # Cappable
///
/// A data stream can be partially read into a `TempFile` even if the incoming
/// stream exceeds the data limit via the [`Capped<TempFile>`] data and form
/// guard.
///
/// # Examples
///
/// **Data Guard**
///
/// ```rust
/// # use rocket::post;
/// use rocket::data::TempFile;
///
/// #[post("/upload", data = "<file>")]
/// async fn upload(mut file: TempFile<'_>) -> std::io::Result<()> {
///     file.persist_to("/tmp/complete/file.txt").await?;
///     Ok(())
/// }
/// ```
///
/// **Form Field**
///
/// ```rust
/// # #[macro_use] extern crate rocket;
/// use rocket::data::TempFile;
/// use rocket::form::Form;
///
/// #[derive(FromForm)]
/// struct Upload<'f> {
///     upload: TempFile<'f>
/// }
///
/// #[post("/form", data = "<form>")]
/// async fn upload(mut form: Form<Upload<'_>>) -> std::io::Result<()> {
///     form.upload.persist_to("/tmp/complete/file.txt").await?;
///     Ok(())
/// }
/// ```
///
/// See also the [`Capped`] documentation for an example of `Capped<TempFile>`
/// as a data guard.
#[derive(Debug)]
pub enum TempFile<'v> {
    #[doc(hidden)]
    File {
        file_name: Option<&'v str>,
        content_type: Option<ContentType>,
        path: Either<TempPath, PathBuf>,
        len: u64,
    },
    #[doc(hidden)]
    Buffered {
        content: &'v str,
    }
}

impl<'v> TempFile<'v> {
    /// Persists the temporary file, moving it to `path`.
    ///
    /// This method _does not_ create a copy of `self`, nor a new link to the
    /// contents of `self`: it renames the temporary file to `path` and marks it
    /// as non-temporary. As a result, this method _cannot_ be used to create
    /// multiple copies of `self`. To create multiple links, use
    /// [`std::fs::hard_link()`] with `path` as the `src` _after_ calling this
    /// method.
    ///
    /// # Example
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::data::TempFile;
    ///
    /// #[post("/", data = "<file>")]
    /// async fn handle(mut file: TempFile<'_>) -> std::io::Result<()> {
    ///     # assert!(file.path().is_none());
    ///     # let some_path = std::env::temp_dir().join("some-file.txt");
    ///     file.persist_to(&some_path).await?;
    ///     assert_eq!(file.path(), Some(&*some_path));
    ///
    ///     Ok(())
    /// }
    /// # let file = TempFile::Buffered { content: "hi".into() };
    /// # rocket::async_test(handle(file)).unwrap();
    /// ```
    pub async fn persist_to<P>(&mut self, path: P) -> io::Result<()>
        where P: AsRef<Path>
    {
        use std::mem::replace;
        use tokio::io::AsyncWriteExt;

        let new_path = path.as_ref();
        match self {
            TempFile::File { path: either, .. } => {
                let path = replace(either, Either::Right(new_path.to_path_buf()));
                match path {
                    Either::Left(temp_path) => {
                        let new_path = new_path.to_path_buf();
                        let result = tokio::task::spawn_blocking(move || {
                            temp_path.persist(new_path)
                        }).await.map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "spawn_block")
                        })?;

                        if let Err(e) = result {
                            *either = Either::Left(e.path);
                            return Err(e.error);
                        }
                    },
                    Either::Right(prev) => {
                        if let Err(e) = fs::rename(&prev, new_path).await {
                            *either = Either::Right(prev);
                            return Err(e);
                        }
                    }
                }
            }
            TempFile::Buffered { content } => {
                let mut file = File::create(new_path).await?;
                file.write_all(content.as_bytes()).await?;
                *self = TempFile::File {
                    file_name: None,
                    content_type: None,
                    path: Either::Right(new_path.to_path_buf()),
                    len: content.len() as u64
                };
            }
        }

        Ok(())
    }

    /// Returns the size, in bytes, of the file.
    ///
    /// This method does not perform any system calls.
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::data::TempFile;
    ///
    /// #[post("/", data = "<file>")]
    /// fn handler(file: TempFile<'_>) {
    ///     let file_len = file.len();
    /// }
    /// ```
    pub fn len(&self) -> u64 {
        match self {
            TempFile::File { len, .. } => *len,
            TempFile::Buffered { content } => content.len() as u64,
        }
    }

    /// Returns the path to the file if it is known.
    ///
    /// Once a file is persisted with [`TempFile::persist_to()`], this method is
    /// guaranteed to return `Some`. Prior to this point, however, this method
    /// may return `Some` or `None`, depending on whether the file is on disk or
    /// partially buffered in memory.
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::data::TempFile;
    ///
    /// #[post("/", data = "<file>")]
    /// async fn handle(mut file: TempFile<'_>) -> std::io::Result<()> {
    ///     # assert!(file.path().is_none());
    ///     # let some_path = std::env::temp_dir().join("some-file.txt");
    ///     file.persist_to(&some_path).await?;
    ///     assert_eq!(file.path(), Some(&*some_path));
    ///
    ///     Ok(())
    /// }
    /// # let file = TempFile::Buffered { content: "hi".into() };
    /// # rocket::async_test(handle(file)).unwrap();
    /// ```
    pub fn path(&self) -> Option<&Path> {
        match self {
            TempFile::File { path: Either::Left(p), .. } => Some(p.as_ref()),
            TempFile::File { path: Either::Right(p), .. } => Some(p.as_path()),
            TempFile::Buffered { .. } => None,
        }
    }

    /// Returns the name of the file as specified in the form field.
    ///
    /// A multipart data form field can optionally specify the name of a file.
    /// A browser will typically send the actual name of a user's selected file
    /// in this field. This method returns that value, if it was specified,
    /// without a file extension.
    ///
    /// The name is guaranteed to be a _true_ filename minus the extension. It
    /// has been sanitized so as to not to contain path components, start with
    /// `.` or `*`, or end with `:`, `>`, or `<`, making it safe for direct use
    /// as the name of a file.
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::data::TempFile;
    ///
    /// #[post("/", data = "<file>")]
    /// async fn handle(mut file: TempFile<'_>) -> std::io::Result<()> {
    ///     # let some_dir = std::env::temp_dir();
    ///     if let Some(name) = file.file_name() {
    ///         // Due to Rocket's sanitization, this is safe.
    ///         file.persist_to(&some_dir.join(name)).await?;
    ///     }
    ///
    ///     Ok(())
    /// }
    /// ```
    pub fn file_name(&self) -> Option<&str> {
        match *self {
            TempFile::File { file_name, .. } => file_name,
            TempFile::Buffered { .. } => None
        }
    }

    /// Returns the Content-Type of the file as specified in the form field.
    ///
    /// A multipart data form field can optionally specify the content-type of a
    /// file. A browser will typically sniff the file's extension to set the
    /// content-type. This method returns that value, if it was specified.
    ///
    /// ```rust
    /// # #[macro_use] extern crate rocket;
    /// use rocket::data::TempFile;
    ///
    /// #[post("/", data = "<file>")]
    /// fn handle(file: TempFile<'_>) {
    ///     let content_type = file.content_type();
    /// }
    /// ```
    pub fn content_type(&self) -> Option<&ContentType> {
        match self {
            TempFile::File { content_type, .. } => content_type.as_ref(),
            TempFile::Buffered { .. } => None
        }
    }

    async fn from<'a>(
        req: &Request<'_>,
        data: Data,
        file_name: Option<&'a str>,
        content_type: Option<ContentType>,
    ) -> io::Result<Capped<TempFile<'a>>> {
        let limit = content_type.as_ref()
            .and_then(|ct| ct.extension())
            .and_then(|ext| req.limits().find(&["file", ext.as_str()]))
            .or_else(|| req.limits().get("file"))
            .unwrap_or(Limits::FILE);

        let temp_dir = req.config().temp_dir.clone();
        let file = tokio::task::spawn_blocking(move || {
            NamedTempFile::new_in(temp_dir)
        }).await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "spawn_block panic")
        })??;

        let (file, temp_path) = file.into_parts();
        let mut file = File::from_std(file);
        let n = data.open(limit).stream_to(tokio::io::BufWriter::new(&mut file)).await?;
        let temp_file = TempFile::File {
            content_type, file_name,
            path: Either::Left(temp_path),
            len: n.written,
        };

        Ok(Capped::new(temp_file, n))
    }
}

#[crate::async_trait]
impl<'v> FromFormField<'v> for Capped<TempFile<'v>> {
    fn from_value(field: ValueField<'v>) -> Result<Self, Errors<'v>> {
        let n = N { written: field.value.len() as u64, complete: true  };
        Ok(Capped::new(TempFile::Buffered { content: field.value }, n))
    }

    async fn from_data(
        f: DataField<'v, '_>
    ) -> Result<Self, Errors<'v>> {
        Ok(TempFile::from(f.request, f.data, f.file_name, Some(f.content_type)).await?)
    }
}

#[crate::async_trait]
impl<'r> FromData<'r> for Capped<TempFile<'_>> {
    type Error = io::Error;

    async fn from_data(
        req: &'r crate::Request<'_>,
        data: crate::Data
    ) -> crate::data::Outcome<Self, Self::Error> {
        TempFile::from(req, data, None, req.content_type().cloned()).await
            .into_outcome(Status::BadRequest)
    }
}

impl_strict_from_form_field_from_capped!(TempFile<'v>);
impl_strict_from_data_from_capped!(TempFile<'_>);
