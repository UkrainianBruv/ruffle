use crate::parse::DocumentHolder;
use crate::recents::{Recent, Recents};
use crate::write::TableExt;
use toml_edit::{value, ArrayOfTables, Table};

pub struct RecentsWriter<'a>(&'a mut DocumentHolder<Recents>);

impl<'a> RecentsWriter<'a> {
    pub fn new(recents: &'a mut DocumentHolder<Recents>) -> Self {
        Self(recents)
    }

    fn with_underlying_table(&mut self, fun: impl FnOnce(&mut Recents, &mut ArrayOfTables)) {
        self.0.edit(|values, toml_document| {
            let table = toml_document.get_or_create_array_of_tables("recent");
            fun(values, table)
        })
    }

    pub fn clear(&mut self) {
        self.with_underlying_table(|values, array| {
            array.clear();
            values.clear();
        });
    }

    /// Pushes a new recent entry on the entry stack, if same entry already exists, it will get moved to the top.
    pub fn push(&mut self, recent: Recent, limit: usize) {
        if limit == 0 {
            // Do not even bother.
            return;
        }

        self.with_underlying_table(|values, array| {
            // First, lets check if we already have existing entry with the same URL and move it to the top.
            let existing = values.iter().position(|x| x.url == recent.url);

            if let Some(index) = existing {
                // Existing entry, just move it to the top.

                // Update TOML first, then internal values.
                // TODO: Unfortunately, ArrayOfTables does not return the removed entry, so we need to recreate it.
                //       https://github.com/toml-rs/toml/issues/712
                array.remove(index);
                let mut table = Table::new();
                table["url"] = value(recent.url.as_str());
                array.push(table);

                let recent = values.remove(index);
                values.push(recent);
            } else {
                // New entry.
                // Evict old entries, if we are at or over the limit.
                if values.len() >= limit {
                    // Remove n elements over limit plus 1, since we need to push a new one too.
                    let elements_to_remove = (values.len() - limit) + 1;

                    // yes, this is inefficient, but this is not hot code :D (usually we only need to remove 1 element, unless the limit changed)
                    for _ in 0..elements_to_remove {
                        array.remove(0);
                        values.remove(0);
                    }
                }

                // Create a new table and push it.
                let mut table = Table::new();
                table["url"] = value(recent.url.as_str());
                array.push(table);
                values.push(recent);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recents::read_recents;
    use url::Url;

    crate::define_serialization_test_helpers!(read_recents, Recents, RecentsWriter);

    #[test]
    fn simple_push() {
        test(
            "",
            |writer| {
                writer.push(
                    Recent {
                        url: Url::parse("file:///1.swf").unwrap(),
                    },
                    10,
                )
            },
            "[[recent]]\nurl = \"file:///1.swf\"\n",
        );
    }

    #[test]
    fn test_limit() {
        test("[[recent]]\nurl = \"file:///1.swf\"\n[[recent]]\nurl = \"file:///2.swf\"\n[[recent]]\nurl = \"file:///3.swf\"\n", |writer| writer.push(Recent {
            url: Url::parse("file:///very_important_file.swf").unwrap(),
        }, 2), "[[recent]]\nurl = \"file:///3.swf\"\n\n[[recent]]\nurl = \"file:///very_important_file.swf\"\n");
    }

    #[test]
    fn test_move_to_top() {
        test("[[recent]]\nurl = \"file:///very_important_file.swf\"\n[[recent]]\nurl = \"file:///2.swf\"\n[[recent]]\nurl = \"file:///3.swf\"\n", |writer| writer.push(Recent {
            url: Url::parse("file:///very_important_file.swf").unwrap(),
        }, 3), "[[recent]]\nurl = \"file:///2.swf\"\n[[recent]]\nurl = \"file:///3.swf\"\n\n[[recent]]\nurl = \"file:///very_important_file.swf\"\n");
    }

    #[test]
    fn clear() {
        test("[[recent]]\nurl = \"file:///file_one.swf\"\n[[recent]]\nurl = \"file:///file_two.swf\"\n[[recent]]\nurl = \"file:///3.swf\"\n", |writer| writer.clear(), "");
    }

    #[test]
    fn zero_limit() {
        test(
            "",
            |writer| {
                writer.push(
                    Recent {
                        url: Url::parse("file:///no_crash.swf").unwrap(),
                    },
                    0,
                )
            },
            "",
        );
    }
}
