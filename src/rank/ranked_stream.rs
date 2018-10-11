use std::ops::Range;
use std::rc::Rc;
use std::{mem, vec, cmp};

use fnv::FnvHashMap;
use fst::Streamer;
use group_by::GroupByMut;

use crate::automaton::{DfaExt, AutomatonExt};
use crate::metadata::Metadata;
use crate::metadata::ops::OpBuilder;
use crate::rank::criterion::Criterion;
use crate::rank::Document;
use crate::Match;

#[derive(Clone)]
pub struct RankedStreamBuilder<'m, C> {
    metadata: &'m Metadata,
    automatons: Vec<Rc<DfaExt>>,
    criteria: Vec<C>,
}

impl<'m, C> RankedStreamBuilder<'m, C> {
    pub fn new(metadata: &'m Metadata, automatons: Vec<DfaExt>) -> Self {
        RankedStreamBuilder {
            metadata: metadata,
            automatons: automatons.into_iter().map(Rc::new).collect(),
            criteria: Vec::new(), // hummm...  prefer the criterion::default() ones !
        }
    }

    pub fn criteria(&mut self, criteria: Vec<C>) {
        self.criteria = criteria;
    }

    pub fn build(&self) -> RankedStream<C> {
        let mut builder = OpBuilder::with_automatons(self.automatons.clone());
        builder.push(self.metadata);

        RankedStream {
            stream: builder.union(),
            automatons: &self.automatons,
            criteria: &self.criteria,
        }
    }
}

pub struct RankedStream<'a, 'm, C> {
    stream: crate::metadata::ops::Union<'m>,
    automatons: &'a [Rc<DfaExt>],
    criteria: &'a [C],
}

impl<'a, 'm, C> RankedStream<'a, 'm, C> {
    pub fn retrieve_documents(&mut self, range: Range<usize>) -> Vec<Document>
    where C: Criterion
    {
        let mut matches = FnvHashMap::default();

        while let Some((string, indexed_values)) = self.stream.next() {
            for iv in indexed_values {
                let automaton = &self.automatons[iv.index];
                let distance = automaton.eval(string).to_u8();
                let is_exact = distance == 0 && string.len() == automaton.query_len();

                for di in iv.doc_indexes.as_slice() {
                    let match_ = Match {
                        query_index: iv.index as u32,
                        distance: distance,
                        attribute: di.attribute,
                        attribute_index: di.attribute_index,
                        is_exact: is_exact,
                    };
                    matches.entry(di.document).or_insert_with(Vec::new).push(match_);
                }
            }
        }

        // collect matches from an HashMap into a Vec
        let mut documents: Vec<_> = matches.into_iter().map(|(id, mut matches)| {
            matches.sort_unstable();
            unsafe { Document::from_sorted_matches(id, matches) }
        }).collect();

        let mut groups = vec![documents.as_mut_slice()];

        for criterion in self.criteria {
            let tmp_groups = mem::replace(&mut groups, Vec::new());
            let mut current_range = Range { start: 0, end: 0 };

            'grp: for group in tmp_groups {
                current_range.end += group.len();

                // if a part of the current group is in the range returned
                // we must sort it and emit the sub-groups
                if current_range.contains(&range.start) {
                    group.sort_unstable_by(|a, b| criterion.evaluate(a, b));
                    for group in GroupByMut::new(group, |a, b| criterion.eq(a, b)) {
                        groups.push(group);
                        if current_range.end >= range.end { break 'grp }
                    }
                } else {
                    groups.push(group)
                }

                current_range.start = current_range.end;
            }
        }

        // TODO find a better algorithm, here we allocate for too many documents
        //      and we do a useless allocation, we should reuse the documents Vec
        let start = cmp::min(range.start, documents.len());
        let mut documents = documents.split_off(start);
        documents.truncate(range.len());
        documents
    }
}
