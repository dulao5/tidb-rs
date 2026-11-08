//! go-file: pkg/types/json_binary_functions.go
//!
//! Functions on binary JSON (exception A4): type, unquote/quote, path extraction, modification,
//! comparison, merge, containment, depth, search and walk. Functions that build bytes follow
//! Go's layout decisions line by line (A4 spec 3.1, 5.1).
//!
//! Go identifies a sub-value by `&bj.Value[0]` (deduplication in `Extract`, the modify target
//! in `binaryModifier`); here it is the address of the first value byte ([`BinaryJSON::ptr`]),
//! the same identity because every view borrows from one document.

#![allow(non_upper_case_globals)]

use std::collections::{HashMap, HashSet};

use tidb_base::gofmt::{Arg, sprintf};
use tidb_base::{Error, mysql};

use super::json_binary::{
    BinaryJSON, JSONValue, create_binary_json, le_u16, le_u32, maxJSONDepth, put_u32, uvarint,
};
use super::json_constants::*;
use super::json_path_expr::{
    ArraySelection, JSONPathExpression, PathExpressionFlagExt, PathLeg, is_ecmascript_identifier,
    json_path_array_index_from_start, jsonPathLegArraySelection, jsonPathLegDoubleAsterisk,
    jsonPathLegKey,
};
use crate::errors::{ErrJSONBadOneOrAllArg, ErrJSONVacuousPath};
use crate::stringutil;

impl BinaryJSON<'_> {
    /// Type returns type of BinaryJSON as string.
    // go: BinaryJSON.Type L1
    pub fn r#type(&self) -> &'static str {
        match self.type_code {
            JSONTypeCodeObject => "OBJECT",
            JSONTypeCodeArray => "ARRAY",
            JSONTypeCodeLiteral => match self.value[0] {
                JSONLiteralNil => "NULL",
                _ => "BOOLEAN",
            },
            JSONTypeCodeInt64 => "INTEGER",
            JSONTypeCodeUint64 => "UNSIGNED INTEGER",
            JSONTypeCodeFloat64 => "DOUBLE",
            JSONTypeCodeString => "STRING",
            JSONTypeCodeOpaque => {
                let typ = self.get_opaque_field_type();
                match typ {
                    mysql::TypeTinyBlob
                    | mysql::TypeMediumBlob
                    | mysql::TypeLongBlob
                    | mysql::TypeBlob
                    | mysql::TypeString
                    | mysql::TypeVarString
                    | mysql::TypeVarchar => "BLOB",
                    mysql::TypeBit => "BIT",
                    _ => "OPAQUE",
                }
            }
            JSONTypeCodeDate => "DATE",
            JSONTypeCodeDatetime => "DATETIME",
            JSONTypeCodeTimestamp => "DATETIME",
            JSONTypeCodeDuration => "TIME",
            _ => {
                let msg = sprintf(unknownTypeCodeErrorMsg, &[Arg::from(self.type_code)]);
                panic!("{}", String::from_utf8_lossy(&msg));
            }
        }
    }

    /// Unquote is for JSON_UNQUOTE.
    // go: BinaryJSON.Unquote L1 [M-ERR]
    pub fn unquote(&self) -> Result<Vec<u8>, Error> {
        match self.type_code {
            JSONTypeCodeString => unquote_string(self.get_string()),
            _ => Ok(self.string()),
        }
    }

    /// Extract receives several path expressions as arguments, matches them in bj, and returns:
    ///
    ///    ret: target JSON matched any path expressions. maybe autowrapped as an array.
    ///    found: true if any path expressions matched.
    ///
    /// Go passes a fresh `dup` set for every path. A path without wildcards, ranges or `**`
    /// reaches at most one value, so it cannot produce a duplicate and gets no set here (saves
    /// the allocation and hashing on the common constant-path case; same result).
    // go: BinaryJSON.Extract L2 (no dup set for a path that cannot match multiple values)
    pub fn extract(&self, path_expr_list: &[JSONPathExpression]) -> (BinaryJSON<'_>, bool) {
        let mut buf: Vec<BinaryJSON<'_>> = Vec::with_capacity(1);
        for path_expr in path_expr_list {
            if path_expr.could_match_multiple_values() {
                let mut dup = HashSet::new();
                buf = self
                    .as_ref()
                    .extract_to(buf, &path_expr.legs, Some(&mut dup), false);
            } else {
                buf = self.as_ref().extract_to(buf, &path_expr.legs, None, false);
            }
        }

        if buf.is_empty() {
            (BinaryJSON::default(), false)
        } else if path_expr_list.len() == 1 && buf.len() == 1 {
            // If pathExpr contains asterisks, len(elemList) won't be 1
            // even if len(pathExprList) equals to 1.
            // Fix https://github.com/pingcap/tidb/issues/30352
            if path_expr_list[0].could_match_multiple_values() {
                return (build_binary_json_array(&buf), true);
            }
            (buf.remove(0), true)
        } else {
            (build_binary_json_array(&buf), true)
        }
    }

    // go: BinaryJSON.extractOne L1
    pub(crate) fn extract_one(&self, path_expr: &JSONPathExpression) -> Vec<BinaryJSON<'_>> {
        let result = Vec::with_capacity(1);
        self.as_ref()
            .extract_to(result, &path_expr.legs, None, true)
    }

    // go: BinaryJSON.objectSearchKey L1
    pub(crate) fn object_search_key(&self, key: &[u8]) -> Option<BinaryJSON<'_>> {
        // objectGetKey on the value bytes, dereferenced once for the whole search.
        let v: &[u8] = &self.value;
        let object_get_key = |i: usize| {
            let e = headerSize + i * keyEntrySize;
            let key_off = le_u32(&v[e..]) as usize;
            let key_len = le_u16(&v[e + keyLenOff..]) as usize;
            &v[key_off..key_off + key_len]
        };
        let elem_count = le_u32(v) as usize;
        let idx = sort_search(elem_count, |i| compare_key(object_get_key(i), key).is_ge());
        if idx < elem_count && compare_key(object_get_key(idx), key).is_eq() {
            return Some(self.object_get_val(idx));
        }
        None
    }

    /// GetElemDepth for JSON_DEPTH
    /// Returns the maximum depth of a JSON document
    /// rules referenced by MySQL JSON_DEPTH function
    /// [https://dev.mysql.com/doc/refman/5.7/en/json-attribute-functions.html#function_json-depth]
    /// 1) An empty array, empty object, or scalar value has depth 1.
    /// 2) A nonempty array containing only elements of depth 1 or nonempty object containing only member values of depth 1 has depth 2.
    /// 3) Otherwise, a JSON document has depth greater than 2.
    ///
    /// e.g. depth of '{}', '[]', 'true': 1
    ///
    /// e.g. depth of '[10, 20]', '[[], {}]': 2
    ///
    /// e.g. depth of '[10, {"a": 20}]': 3
    // go: BinaryJSON.GetElemDepth L1
    pub fn get_elem_depth(&self) -> isize {
        match self.type_code {
            JSONTypeCodeObject => {
                let elem_count = self.get_elem_count();
                let mut max_depth = 0;
                for i in 0..elem_count {
                    let obj = self.object_get_val(i);
                    let depth = obj.get_elem_depth();
                    if depth > max_depth {
                        max_depth = depth;
                    }
                }
                max_depth + 1
            }
            JSONTypeCodeArray => {
                let elem_count = self.get_elem_count();
                let mut max_depth = 0;
                for i in 0..elem_count {
                    let obj = self.array_get_elem(i);
                    let depth = obj.get_elem_depth();
                    if depth > max_depth {
                        max_depth = depth;
                    }
                }
                max_depth + 1
            }
            _ => 1,
        }
    }

    /// Modify modifies a JSON object by insert, replace or set.
    /// All path expressions cannot contain * or ** wildcard.
    /// If any error occurs, the input won't be changed.
    // go: BinaryJSON.Modify L1 [M-ERR]
    pub fn modify(
        &self,
        path_expr_list: &[JSONPathExpression],
        values: &[BinaryJSON<'_>],
        mt: JSONModifyType,
    ) -> Result<BinaryJSON<'static>, Error> {
        if path_expr_list.len() != values.len() {
            // TODO: should return 1582(42000)
            return Err(Error::plain("Incorrect parameter count"));
        }
        for path_expr in path_expr_list {
            if path_expr.flags.contains_any_asterisk() || path_expr.flags.contains_any_range() {
                return Err(ErrInvalidJSONPathMultipleSelection.clone());
            }
        }
        let mut bj = self.copy();
        for i in 0..path_expr_list.len() {
            let (path_expr, value) = (&path_expr_list[i], &values[i]);
            let mut modifier = BinaryModifier::new(bj.as_ref());
            let next = match mt {
                JSONModifyInsert => modifier.insert(path_expr, value),
                JSONModifyReplace => modifier.replace(path_expr, value),
                JSONModifySet => modifier.set(path_expr, value),
                _ => bj.copy(),
            };
            if let Some(err) = modifier.err {
                return Err(err);
            }
            bj = next;
        }
        if bj.get_elem_depth() - 1 > maxJSONDepth {
            return Err(ErrJSONDocumentTooDeep.clone());
        }
        Ok(bj)
    }

    /// ArrayInsert insert a BinaryJSON into the given array cell.
    /// All path expressions cannot contain * or ** wildcard.
    /// If any error occurs, the input won't be changed.
    // go: BinaryJSON.ArrayInsert L1 [M-ERR]
    pub fn array_insert(
        &self,
        path_expr: &JSONPathExpression,
        value: &BinaryJSON<'_>,
    ) -> Result<BinaryJSON<'static>, Error> {
        // Check the path is a index
        if path_expr.legs.is_empty() {
            return Err(ErrInvalidJSONPathArrayCell.clone());
        }
        let (parent_path, last_leg) = path_expr.pop_one_last_leg();
        if last_leg.typ != jsonPathLegArraySelection {
            return Err(ErrInvalidJSONPathArrayCell.clone());
        }
        // Find the target array
        let (obj, exists) = self.extract(std::slice::from_ref(&parent_path));
        if !exists || obj.type_code != JSONTypeCodeArray {
            return Ok(self.copy());
        }

        let idx = match last_leg.array_selection {
            Some(ArraySelection::Index(index)) => index.get_index_from_start(&obj),
            _ => return Err(ErrInvalidJSONPathArrayCell.clone()),
        };
        let count = obj.get_elem_count() as isize;
        let idx = if idx >= count { count } else { idx };
        // Insert into the array
        let mut new_array: Vec<BinaryJSON<'_>> = Vec::with_capacity(count as usize + 1);
        for i in 0..idx {
            new_array.push(obj.array_get_elem(i as usize));
        }
        new_array.push(value.as_ref());
        for i in idx..count {
            new_array.push(obj.array_get_elem(i as usize));
        }
        let obj = build_binary_json_array(&new_array);

        self.modify(std::slice::from_ref(&parent_path), &[obj], JSONModifySet)
    }

    /// Remove removes the elements indicated by pathExprList from JSON.
    // go: BinaryJSON.Remove L1 [M-ERR]
    pub fn remove(
        &self,
        path_expr_list: &[JSONPathExpression],
    ) -> Result<BinaryJSON<'static>, Error> {
        let mut bj = self.copy();
        for path_expr in path_expr_list {
            if path_expr.legs.is_empty() {
                return Err(ErrJSONVacuousPath.clone());
            }
            if path_expr.flags.contains_any_asterisk() || path_expr.flags.contains_any_range() {
                return Err(ErrInvalidJSONPathMultipleSelection.clone());
            }
            let mut modifer = BinaryModifier::new(bj.as_ref());
            let next = modifer.remove(path_expr);
            if let Some(err) = modifer.err {
                return Err(err);
            }
            bj = next;
        }
        Ok(bj)
    }

    /// Search for JSON_Search
    /// rules referenced by MySQL JSON_SEARCH function
    /// [https://dev.mysql.com/doc/refman/5.7/en/json-search-functions.html#function_json-search]
    ///
    /// Go returns `(res, isNull, err)`; an error here means `isNull` is true.
    // go: BinaryJSON.Search L1 [M-ERRVAL]
    pub fn search(
        &self,
        contain_type: &str,
        search: &[u8],
        escape: u8,
        path_expres: &[JSONPathExpression],
    ) -> Result<(BinaryJSON<'static>, bool), Error> {
        if contain_type != JSONContainsPathOne && contain_type != JSONContainsPathAll {
            return Err(
                ErrJSONBadOneOrAllArg.gen_with_stack_by_args(vec![Arg::from("json_search")])
            );
        }
        let (pat_chars, pat_types) = stringutil::compile_pattern(search, escape);

        let mut result: Vec<Vec<u8>> = Vec::new();
        let mut walk_fn =
            |fullpath: &JSONPathExpression, bj: BinaryJSON<'_>| -> Result<bool, Error> {
                if bj.type_code == JSONTypeCodeString
                    && stringutil::do_match(bj.get_string(), &pat_chars, &pat_types)
                {
                    result.push(fullpath.string());
                    if contain_type == JSONContainsPathOne {
                        return Ok(true);
                    }
                }
                Ok(false)
            };
        self.walk(&mut walk_fn, path_expres)?;
        match result.len() {
            0 => Ok((BinaryJSON::default(), true)),
            1 => Ok((
                create_binary_json(JSONValue::String(result.remove(0))),
                false,
            )),
            _ => Ok((
                create_binary_json(JSONValue::Array(
                    result.into_iter().map(JSONValue::String).collect(),
                )),
                false,
            )),
        }
    }

    /// Walk traverse BinaryJSON objects
    // go: BinaryJSON.Walk L1 [M-ERR]
    pub fn walk(
        &self,
        walk_fn: &mut dyn FnMut(&JSONPathExpression, BinaryJSON<'_>) -> Result<bool, Error>,
        path_expr_list: &[JSONPathExpression],
    ) -> Result<(), Error> {
        let mut walker = Walker {
            path_set: HashSet::new(),
            walk_fn,
        };
        let fullpath = JSONPathExpression {
            legs: Vec::with_capacity(32),
            flags: 0,
        };
        if !path_expr_list.is_empty() {
            for path_expr in path_expr_list {
                let stop = self.as_ref().extract_to_callback(
                    path_expr,
                    &mut |p, bj| walker.do_walk(p, bj),
                    &fullpath,
                )?;
                if stop {
                    return Ok(());
                }
            }
        } else {
            walker.do_walk(&fullpath, self.as_ref())?;
        }
        Ok(())
    }
}

impl<'a> BinaryJSON<'a> {
    // go: BinaryJSON.extractTo L1
    #[allow(clippy::collapsible_match)] // Go's switch with an if inside each case
    fn extract_to(
        self,
        mut buf: Vec<BinaryJSON<'a>>,
        legs: &[PathLeg],
        mut dup: Option<&mut HashSet<usize>>,
        one: bool,
    ) -> Vec<BinaryJSON<'a>> {
        if legs.is_empty() {
            if let Some(dup) = dup {
                if dup.contains(&self.ptr()) {
                    return buf;
                }
                dup.insert(self.ptr());
            }
            buf.push(self);
            return buf;
        }
        // Go's popOneLeg: the first leg and the rest (its flags are not used here).
        let (current_leg, sub_path_expr) = (&legs[0], &legs[1..]);
        if current_leg.typ == jsonPathLegArraySelection {
            if self.type_code != JSONTypeCodeArray {
                // If the current object is not an array, still append them if the selection includes
                // 0. But for asterisk, it still returns NULL.
                //
                // don't call `getIndexRange` or `getIndexFromStart`, they will panic if the argument
                // is not array.
                match &current_leg.array_selection {
                    Some(ArraySelection::Index(index)) => {
                        if index.0 == 0 {
                            buf = self.extract_to(buf, sub_path_expr, dup, one);
                        }
                    }
                    Some(ArraySelection::Range(start, end)) => {
                        // for [0 to Non-negative Number] and [0 to last], it extracts itself
                        if start.0 == 0 && end.0 >= -1 {
                            buf = self.extract_to(buf, sub_path_expr, dup, one);
                        }
                    }
                    _ => {}
                }
                return buf;
            }

            let (start, end) = current_leg
                .array_selection
                .as_ref()
                .expect("array leg")
                .get_index_range(&self);
            if start >= 0 && start <= end {
                for i in start..=end {
                    let elem = borrow_elem(&self, |b| b.array_get_elem(i as usize));
                    buf = elem.extract_to(buf, sub_path_expr, dup.as_deref_mut(), one);
                }
            }
        } else if current_leg.typ == jsonPathLegKey && self.type_code == JSONTypeCodeObject {
            let elem_count = self.get_elem_count();
            if current_leg.dot_key == b"*" {
                let mut i = 0;
                while i < elem_count && !json_finished(&buf, one) {
                    let elem = borrow_elem(&self, |b| b.object_get_val(i));
                    buf = elem.extract_to(buf, sub_path_expr, dup.as_deref_mut(), one);
                    i += 1;
                }
            } else {
                let child = borrow_opt(&self, |b| b.object_search_key(&current_leg.dot_key));
                if let Some(child) = child {
                    buf = child.extract_to(buf, sub_path_expr, dup, one);
                }
            }
        } else if current_leg.typ == jsonPathLegDoubleAsterisk {
            buf = self
                .clone()
                .extract_to(buf, sub_path_expr, dup.as_deref_mut(), one);
            if self.type_code == JSONTypeCodeArray {
                let elem_count = self.get_elem_count();
                let mut i = 0;
                while i < elem_count && !json_finished(&buf, one) {
                    let elem = borrow_elem(&self, |b| b.array_get_elem(i));
                    buf = elem.extract_to(buf, legs, dup.as_deref_mut(), one);
                    i += 1;
                }
            } else if self.type_code == JSONTypeCodeObject {
                let elem_count = self.get_elem_count();
                let mut i = 0;
                while i < elem_count && !json_finished(&buf, one) {
                    let elem = borrow_elem(&self, |b| b.object_get_val(i));
                    buf = elem.extract_to(buf, legs, dup.as_deref_mut(), one);
                    i += 1;
                }
            }
        }
        buf
    }

    /// extractToCallback callback alternative of extractTo
    ///
    ///    would be more effective when walk through the whole JSON is unnecessary
    ///
    /// NOTICE: path [0] & [*] for JSON object other than array is INVALID, which is different from extractTo.
    // go: BinaryJSON.extractToCallback L1 [M-ERR]
    fn extract_to_callback(
        self,
        path_expr: &JSONPathExpression,
        callback_fn: &mut dyn FnMut(&JSONPathExpression, BinaryJSON<'_>) -> Result<bool, Error>,
        fullpath: &JSONPathExpression,
    ) -> Result<bool, Error> {
        if path_expr.legs.is_empty() {
            return callback_fn(fullpath, self);
        }

        let (current_leg, sub_path_expr) = path_expr.pop_one_leg();
        if current_leg.typ == jsonPathLegArraySelection && self.type_code == JSONTypeCodeArray {
            let elem_count = self.get_elem_count() as isize;
            match current_leg.array_selection.as_ref().expect("array leg") {
                ArraySelection::Asterisk => {
                    for i in 0..elem_count {
                        let path = fullpath.push_back_one_array_selection_leg(
                            ArraySelection::Index(json_path_array_index_from_start(i)),
                        );
                        let elem = borrow_elem(&self, |b| b.array_get_elem(i as usize));
                        if elem.extract_to_callback(&sub_path_expr, callback_fn, &path)? {
                            return Ok(true);
                        }
                    }
                }
                sel @ ArraySelection::Index(index) => {
                    let idx = index.get_index_from_start(&self);
                    if idx < elem_count && idx >= 0 {
                        let path = fullpath.push_back_one_array_selection_leg(sel.clone());
                        let elem = borrow_elem(&self, |b| b.array_get_elem(idx as usize));
                        if elem.extract_to_callback(&sub_path_expr, callback_fn, &path)? {
                            return Ok(true);
                        }
                    }
                }
                ArraySelection::Range(start, end) => {
                    let start = start.get_index_from_start(&self);
                    let mut end = end.get_index_from_start(&self);
                    if end >= elem_count {
                        end = elem_count - 1;
                    }
                    if start <= end && start >= 0 {
                        for i in start..=end {
                            let path = fullpath.push_back_one_array_selection_leg(
                                ArraySelection::Index(json_path_array_index_from_start(i)),
                            );
                            let elem = borrow_elem(&self, |b| b.array_get_elem(i as usize));
                            if elem.extract_to_callback(&sub_path_expr, callback_fn, &path)? {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        } else if current_leg.typ == jsonPathLegKey && self.type_code == JSONTypeCodeObject {
            let elem_count = self.get_elem_count();
            if current_leg.dot_key == b"*" {
                for i in 0..elem_count {
                    let path = fullpath.push_back_one_key_leg(self.object_get_key(i).to_vec());
                    let elem = borrow_elem(&self, |b| b.object_get_val(i));
                    if elem.extract_to_callback(&sub_path_expr, callback_fn, &path)? {
                        return Ok(true);
                    }
                }
            } else {
                let child = borrow_opt(&self, |b| b.object_search_key(&current_leg.dot_key));
                if let Some(child) = child {
                    let path = fullpath.push_back_one_key_leg(current_leg.dot_key.clone());
                    if child.extract_to_callback(&sub_path_expr, callback_fn, &path)? {
                        return Ok(true);
                    }
                }
            }
        } else if current_leg.typ == jsonPathLegDoubleAsterisk {
            if self
                .clone()
                .extract_to_callback(&sub_path_expr, callback_fn, fullpath)?
            {
                return Ok(true);
            }

            if self.type_code == JSONTypeCodeArray {
                let elem_count = self.get_elem_count() as isize;
                for i in 0..elem_count {
                    let path = fullpath.push_back_one_array_selection_leg(ArraySelection::Index(
                        json_path_array_index_from_start(i),
                    ));
                    let elem = borrow_elem(&self, |b| b.array_get_elem(i as usize));
                    if elem.extract_to_callback(path_expr, callback_fn, &path)? {
                        return Ok(true);
                    }
                }
            } else if self.type_code == JSONTypeCodeObject {
                let elem_count = self.get_elem_count();
                for i in 0..elem_count {
                    let path = fullpath.push_back_one_key_leg(self.object_get_key(i).to_vec());
                    let elem = borrow_elem(&self, |b| b.object_get_val(i));
                    if elem.extract_to_callback(path_expr, callback_fn, &path)? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

/// A child view of `parent` with the parent's lifetime (the child borrows the same bytes).
// glue: re-borrow a sub-value with the document lifetime
fn borrow_elem<'a>(
    parent: &BinaryJSON<'a>,
    f: impl for<'b> FnOnce(&'b BinaryJSON<'a>) -> BinaryJSON<'b>,
) -> BinaryJSON<'a> {
    let child = f(parent);
    let tc = child.type_code;
    let (off, len) = sub_range(parent, &child.value);
    BinaryJSON::borrowed(tc, &doc_bytes(parent)[off..off + len])
}

// glue: re-borrow an optional sub-value with the document lifetime
fn borrow_opt<'a>(
    parent: &BinaryJSON<'a>,
    f: impl for<'b> FnOnce(&'b BinaryJSON<'a>) -> Option<BinaryJSON<'b>>,
) -> Option<BinaryJSON<'a>> {
    let child = f(parent)?;
    let tc = child.type_code;
    let (off, len) = sub_range(parent, &child.value);
    Some(BinaryJSON::borrowed(tc, &doc_bytes(parent)[off..off + len]))
}

// glue: offset of a sub-slice in its parent's bytes
fn sub_range(parent: &BinaryJSON<'_>, child: &[u8]) -> (usize, usize) {
    let off = child.as_ptr() as usize - parent.value.as_ptr() as usize;
    (off, child.len())
}

/// The parent's bytes with lifetime `'a`. Views always borrow (`extract_to` and the walk start
/// from `as_ref()`), so the `Cow` is borrowed.
// glue: lifetime of a borrowed view
fn doc_bytes<'a>(parent: &BinaryJSON<'a>) -> &'a [u8] {
    match &parent.value {
        std::borrow::Cow::Borrowed(b) => b,
        std::borrow::Cow::Owned(_) => panic!("a sub-value view must borrow its document"),
    }
}

struct Walker<'f> {
    path_set: HashSet<Vec<u8>>,
    walk_fn: &'f mut dyn FnMut(&JSONPathExpression, BinaryJSON<'_>) -> Result<bool, Error>,
}

impl Walker<'_> {
    // go-part: BinaryJSON.Walk (the doWalk closure)
    fn do_walk(
        &mut self,
        fullpath: &JSONPathExpression,
        bj: BinaryJSON<'_>,
    ) -> Result<bool, Error> {
        let path_str = fullpath.string();
        if self.path_set.contains(&path_str) {
            return Ok(false);
        }

        let stop = (self.walk_fn)(fullpath, bj.as_ref())?;
        self.path_set.insert(path_str);
        if stop {
            return Ok(true);
        }

        if bj.type_code == JSONTypeCodeArray {
            let elem_count = bj.get_elem_count() as isize;
            for i in 0..elem_count {
                let path = fullpath.push_back_one_array_selection_leg(ArraySelection::Index(
                    json_path_array_index_from_start(i),
                ));
                if self.do_walk(&path, bj.array_get_elem(i as usize))? {
                    return Ok(true);
                }
            }
        } else if bj.type_code == JSONTypeCodeObject {
            let elem_count = bj.get_elem_count();
            for i in 0..elem_count {
                let path = fullpath.push_back_one_key_leg(bj.object_get_key(i).to_vec());
                if self.do_walk(&path, bj.object_get_val(i))? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// UnquoteString remove quotes in a string,
/// including the quotes at the head and tail of string.
// go: UnquoteString L1 [M-ERR]
pub fn unquote_string(str: &[u8]) -> Result<Vec<u8>, Error> {
    let str_len = str.len();
    if str_len < 2 {
        return Ok(str.to_vec());
    }
    let (head, tail) = (str[0], str[str_len - 1]);
    if head == b'"' && tail == b'"' {
        // Remove prefix and suffix '"' before unquoting
        return unquote_json_string(&str[1..str_len - 1]);
    }
    // if value is not double quoted, do nothing
    Ok(str.to_vec())
}

/// unquoteJSONString recognizes the escape sequences shown in:
/// https://dev.mysql.com/doc/refman/5.7/en/json-modification-functions.html#json-unquote-character-escape-sequences
// go: unquoteJSONString L1 [M-ERR]
pub(crate) fn unquote_json_string(s: &[u8]) -> Result<Vec<u8>, Error> {
    let mut ret = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' {
            i += 1;
            if i == s.len() {
                return Err(Error::plain("Missing a closing quotation mark in string"));
            }
            match s[i] {
                b'"' => ret.push(b'"'),
                b'b' => ret.push(0x08),
                b'f' => ret.push(0x0c),
                b'n' => ret.push(b'\n'),
                b'r' => ret.push(b'\r'),
                b't' => ret.push(b'\t'),
                b'\\' => ret.push(b'\\'),
                b'u' => {
                    if i + 4 > s.len() {
                        let mut msg = b"Invalid unicode: ".to_vec();
                        msg.extend_from_slice(&s[i + 1..]);
                        return Err(Error::plain(msg));
                    }
                    let (char, size) = decode_escaped_unicode(&s[i + 1..i + 5])?;
                    ret.extend_from_slice(&char[0..size]);
                    i += 4;
                }
                // For all other escape sequences, backslash is ignored.
                c => ret.push(c),
            }
        } else {
            ret.push(s[i]);
        }
        i += 1;
    }
    Ok(ret)
}

/// decodeEscapedUnicode decodes unicode into utf8 bytes specified in RFC 3629.
/// According RFC 3629, the max length of utf8 characters is 4 bytes.
/// And MySQL use 4 bytes to represent the unicode which must be in [0, 65536).
///
/// A surrogate code point (`\ud800`–`\udfff`) has no UTF-8 encoding: Go's `utf8.RuneLen`
/// returns -1 and the slice `char[0:-1]` panics; so does this function.
// go: decodeEscapedUnicode L1 [M-ERR]
fn decode_escaped_unicode(s: &[u8]) -> Result<([u8; 4], usize), Error> {
    let mut char = [0u8; 4];
    // hex.Decode(char[0:2], s)
    let mut j = 1;
    let mut n = 0;
    while j < s.len() {
        let (p, q) = (s[j - 1], s[j]);
        let a = from_hex_char(p);
        let b = from_hex_char(q);
        if a.is_none() {
            return Err(hex_invalid_byte(p));
        }
        if b.is_none() {
            return Err(hex_invalid_byte(q));
        }
        char[n] = (a.unwrap_or(0) << 4) | b.unwrap_or(0);
        n += 1;
        j += 2;
    }

    let unicode = u16::from_be_bytes([char[0], char[1]]) as u32;
    let Some(c) = char::from_u32(unicode) else {
        panic!("runtime error: slice bounds out of range [:-1]");
    };
    let size = c.len_utf8();
    c.encode_utf8(&mut char[0..size]);
    Ok((char, size))
}

// go-std: encoding/hex.reverseHexTable L1 (lookup as a match)
fn from_hex_char(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// go-std: encoding/hex.InvalidByteError.Error L1
fn hex_invalid_byte(b: u8) -> Error {
    Error::plain(sprintf(
        "encoding/hex: invalid byte: %#U",
        &[Arg::from(i32::from(b))],
    ))
}

/// quoteJSONString escapes interior quote and other characters for JSON_QUOTE
/// https://dev.mysql.com/doc/refman/5.7/en/json-creation-functions.html#function_json-quote
/// TODO: add JSON_QUOTE builtin
// go: quoteJSONString L1
pub(crate) fn quote_json_string(s: &[u8]) -> Vec<u8> {
    // glue: the escapeByteMap map literal of quoteJSONString as a match
    fn escape_byte(b: u8) -> Option<&'static [u8]> {
        match b {
            b'\\' => Some(b"\\\\"),
            b'"' => Some(b"\\\""),
            0x08 => Some(b"\\b"),
            0x0c => Some(b"\\f"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            b'\t' => Some(b"\\t"),
            _ => None,
        }
    }

    let mut ret = Vec::with_capacity(s.len() + 2);
    ret.push(b'"');

    let mut start = 0;
    let mut has_escaped = false;

    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if b < 0x80 {
            if let Some(escaped) = escape_byte(b) {
                if start < i {
                    ret.extend_from_slice(&s[start..i]);
                }
                has_escaped = true;
                ret.extend_from_slice(escaped);
                i += 1;
                start = i;
            } else {
                i += 1;
            }
        } else {
            let (c, size) = tidb_base::gostd::decode_rune(&s[i..]);
            if c == '\u{FFFD}' && size == 1 {
                // refer to codes of `binary.jsonMarshalStringTo`
                if start < i {
                    ret.extend_from_slice(&s[start..i]);
                }
                has_escaped = true;
                ret.extend_from_slice(b"\\ufffd");
                i += size;
                start = i;
                continue;
            }
            i += size;
        }
    }

    if start < s.len() {
        ret.extend_from_slice(&s[start..]);
    }

    if has_escaped || !is_ecmascript_identifier(s) {
        ret.push(b'"');
        return ret;
    }
    ret.remove(0);
    ret
}

// go: jsonFinished L1
fn json_finished(buf: &[BinaryJSON<'_>], one: bool) -> bool {
    one && !buf.is_empty()
}

/// Byte-wise comparison of object keys (Go's `string` comparison, `bytes.Compare`). Keys
/// are mostly short, where a plain loop beats a call to `memcmp` (most of the time of a
/// constant-path `Extract` went to it).
// go-std: bytes.Compare L1
#[inline]
fn compare_key(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    if n > 32 {
        return a.cmp(b);
    }
    for i in 0..n {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    a.len().cmp(&b.len())
}

/// `sort.Search(n, f)`: the smallest index in [0, n) at which f is true, or n.
// go-std: sort.Search L1
fn sort_search(n: usize, f: impl Fn(usize) -> bool) -> usize {
    let (mut i, mut j) = (0usize, n);
    while i < j {
        let h = (i + j) >> 1;
        if !f(h) {
            i = h + 1;
        } else {
            j = h;
        }
    }
    i
}

// go: buildBinaryJSONArray L1
pub(crate) fn build_binary_json_array(elems: &[BinaryJSON<'_>]) -> BinaryJSON<'static> {
    let mut total_size = headerSize + elems.len() * valEntrySize;
    for elem in elems {
        if elem.type_code != JSONTypeCodeLiteral {
            total_size += elem.value.len();
        }
    }
    let mut buf = vec![0u8; headerSize + elems.len() * valEntrySize];
    buf.reserve(total_size - buf.len());
    put_u32(&mut buf, elems.len() as u32);
    put_u32(&mut buf[dataSizeOff..], total_size as u32);
    build_binary_json_elements(&mut buf, headerSize, elems);
    BinaryJSON::owned(JSONTypeCodeArray, buf)
}

// go: buildBinaryJSONElements L1
fn build_binary_json_elements(buf: &mut Vec<u8>, entry_start: usize, elems: &[BinaryJSON<'_>]) {
    for (i, elem) in elems.iter().enumerate() {
        buf[entry_start + i * valEntrySize] = elem.type_code;
        if elem.type_code == JSONTypeCodeLiteral {
            buf[entry_start + i * valEntrySize + valTypeSize] = elem.value[0];
        } else {
            let off = buf.len() as u32;
            put_u32(
                &mut buf[entry_start + i * valEntrySize + valTypeSize..],
                off,
            );
            buf.extend_from_slice(&elem.value);
        }
    }
}

// go: buildBinaryJSONObject L1 [M-ERR]
pub(crate) fn build_binary_json_object(
    keys: &[&[u8]],
    elems: &[BinaryJSON<'_>],
) -> Result<BinaryJSON<'static>, Error> {
    let mut total_size = headerSize + elems.len() * (keyEntrySize + valEntrySize);
    for (i, elem) in elems.iter().enumerate() {
        if elem.type_code != JSONTypeCodeLiteral {
            total_size += elem.value.len();
        }
        total_size += keys[i].len();
    }
    let mut buf = vec![0u8; headerSize + elems.len() * (keyEntrySize + valEntrySize)];
    buf.reserve(total_size - buf.len());
    put_u32(&mut buf, elems.len() as u32);
    put_u32(&mut buf[dataSizeOff..], total_size as u32);
    for (i, key) in keys.iter().enumerate() {
        if key.len() > u16::MAX as usize {
            return Err(ErrJSONObjectKeyTooLong.clone());
        }
        let off = buf.len() as u32;
        put_u32(&mut buf[headerSize + i * keyEntrySize..], off);
        buf[headerSize + i * keyEntrySize + keyLenOff
            ..headerSize + i * keyEntrySize + keyLenOff + 2]
            .copy_from_slice(&(key.len() as u16).to_le_bytes());
        buf.extend_from_slice(key);
    }
    let entry_start = headerSize + elems.len() * keyEntrySize;
    build_binary_json_elements(&mut buf, entry_start, elems);
    Ok(BinaryJSON::owned(JSONTypeCodeObject, buf))
}

/// `binaryModifier`: the document, the modify target (by the address of its first byte, Go's
/// `*byte`) and the new value of the target.
struct BinaryModifier<'a> {
    bj: BinaryJSON<'a>,
    modify_ptr: Option<usize>,
    modify_value: BinaryJSON<'static>,
    err: Option<Error>,
}

impl<'a> BinaryModifier<'a> {
    // glue: the binaryModifier{bj: bj} literal
    fn new(bj: BinaryJSON<'a>) -> BinaryModifier<'a> {
        BinaryModifier {
            bj,
            modify_ptr: None,
            modify_value: BinaryJSON::default(),
            err: None,
        }
    }

    // go: binaryModifier.set L1
    fn set(&mut self, path: &JSONPathExpression, new_bj: &BinaryJSON<'_>) -> BinaryJSON<'static> {
        let result = self.bj.extract_one(path);
        if let Some(first) = result.first() {
            self.modify_ptr = Some(first.ptr());
            self.modify_value = new_bj.copy();
            return self.rebuild();
        }
        self.do_insert(path, new_bj);
        if self.err.is_some() {
            return BinaryJSON::default();
        }
        self.rebuild()
    }

    // go: binaryModifier.replace L1
    fn replace(
        &mut self,
        path: &JSONPathExpression,
        new_bj: &BinaryJSON<'_>,
    ) -> BinaryJSON<'static> {
        let result = self.bj.extract_one(path);
        let Some(first) = result.first() else {
            return self.bj.copy();
        };
        self.modify_ptr = Some(first.ptr());
        self.modify_value = new_bj.copy();
        self.rebuild()
    }

    // go: binaryModifier.insert L1
    fn insert(
        &mut self,
        path: &JSONPathExpression,
        new_bj: &BinaryJSON<'_>,
    ) -> BinaryJSON<'static> {
        let result = self.bj.extract_one(path);
        if !result.is_empty() {
            return self.bj.copy();
        }
        self.do_insert(path, new_bj);
        if self.err.is_some() {
            return BinaryJSON::default();
        }
        self.rebuild()
    }

    /// doInsert inserts the newBj to its parent, and builds the new parent.
    // go: binaryModifier.doInsert L1
    fn do_insert(&mut self, path: &JSONPathExpression, new_bj: &BinaryJSON<'_>) {
        let (parent_path, last_leg) = path.pop_one_last_leg();
        let bj = self.bj.clone();
        let result = bj.extract_one(&parent_path);
        let Some(parent_bj) = result.first() else {
            return;
        };
        if last_leg.typ == jsonPathLegArraySelection {
            self.modify_ptr = Some(parent_bj.ptr());
            if parent_bj.type_code != JSONTypeCodeArray {
                self.modify_value = build_binary_json_array(&[parent_bj.as_ref(), new_bj.as_ref()]);
                return;
            }
            let elem_count = parent_bj.get_elem_count();
            let mut elems = Vec::with_capacity(elem_count + 1);
            for i in 0..elem_count {
                elems.push(parent_bj.array_get_elem(i));
            }
            elems.push(new_bj.as_ref());
            self.modify_value = build_binary_json_array(&elems);
            return;
        }
        if parent_bj.type_code != JSONTypeCodeObject {
            return;
        }
        self.modify_ptr = Some(parent_bj.ptr());
        let elem_count = parent_bj.get_elem_count();
        let insert_key: &[u8] = &last_leg.dot_key;
        let insert_idx = sort_search(elem_count, |i| parent_bj.object_get_key(i) >= insert_key);
        let mut keys: Vec<&[u8]> = Vec::with_capacity(elem_count + 1);
        let mut elems = Vec::with_capacity(elem_count + 1);
        for i in 0..elem_count {
            if i == insert_idx {
                keys.push(insert_key);
                elems.push(new_bj.as_ref());
            }
            keys.push(parent_bj.object_get_key(i));
            elems.push(parent_bj.object_get_val(i));
        }
        if insert_idx == elem_count {
            keys.push(insert_key);
            elems.push(new_bj.as_ref());
        }
        match build_binary_json_object(&keys, &elems) {
            Ok(v) => self.modify_value = v,
            Err(e) => {
                self.modify_value = BinaryJSON::default();
                self.err = Some(e);
            }
        }
    }

    // go: binaryModifier.remove L1
    fn remove(&mut self, path: &JSONPathExpression) -> BinaryJSON<'static> {
        let result = self.bj.extract_one(path);
        if result.is_empty() {
            return self.bj.copy();
        }
        self.do_remove(path);
        if self.err.is_some() {
            return BinaryJSON::default();
        }
        self.rebuild()
    }

    // go: binaryModifier.doRemove L1
    fn do_remove(&mut self, path: &JSONPathExpression) {
        let (parent_path, last_leg) = path.pop_one_last_leg();
        let bj = self.bj.clone();
        let result = bj.extract_one(&parent_path);
        let Some(parent_bj) = result.first() else {
            return;
        };
        if last_leg.typ == jsonPathLegArraySelection {
            if parent_bj.type_code != JSONTypeCodeArray {
                return;
            }
            let Some(ArraySelection::Index(index)) = &last_leg.array_selection else {
                return;
            };
            let idx = index.get_index_from_start(parent_bj);

            self.modify_ptr = Some(parent_bj.ptr());
            let elem_count = parent_bj.get_elem_count();
            let mut elems = Vec::with_capacity(elem_count.saturating_sub(1));
            for i in 0..elem_count {
                if i as isize != idx {
                    elems.push(parent_bj.array_get_elem(i));
                }
            }
            self.modify_value = build_binary_json_array(&elems);
            return;
        }
        if parent_bj.type_code != JSONTypeCodeObject {
            return;
        }
        self.modify_ptr = Some(parent_bj.ptr());
        let elem_count = parent_bj.get_elem_count();
        let remove_key: &[u8] = &last_leg.dot_key;
        let mut keys: Vec<&[u8]> = Vec::with_capacity(elem_count + 1);
        let mut elems = Vec::with_capacity(elem_count + 1);
        for i in 0..elem_count {
            let key = parent_bj.object_get_key(i);
            if key != remove_key {
                keys.push(parent_bj.object_get_key(i));
                elems.push(parent_bj.object_get_val(i));
            }
        }
        match build_binary_json_object(&keys, &elems) {
            Ok(v) => self.modify_value = v,
            Err(e) => {
                self.modify_value = BinaryJSON::default();
                self.err = Some(e);
            }
        }
    }

    /// rebuild merges the old and the modified JSON into a new BinaryJSON
    // go: binaryModifier.rebuild L1
    fn rebuild(&mut self) -> BinaryJSON<'static> {
        let mut buf = Vec::with_capacity(self.bj.value.len() + self.modify_value.value.len());
        let bj = self.bj.clone();
        let tp_code = self.rebuild_to(&mut buf, bj.as_ref());
        BinaryJSON::owned(tp_code, buf)
    }

    /// `rebuildTo` with `bm.bj` as the argument `bj` (Go reassigns the field while recursing).
    // go: binaryModifier.rebuildTo L1
    fn rebuild_to(&mut self, buf: &mut Vec<u8>, bj: BinaryJSON<'_>) -> JSONTypeCode {
        if self.modify_ptr == Some(bj.ptr()) {
            self.modify_ptr = None;
            buf.extend_from_slice(&self.modify_value.value);
            return self.modify_value.type_code;
        } else if self.modify_ptr.is_none() {
            buf.extend_from_slice(&bj.value);
            return bj.type_code;
        }
        if bj.type_code != JSONTypeCodeArray && bj.type_code != JSONTypeCodeObject {
            buf.extend_from_slice(&bj.value);
            return bj.type_code;
        }
        let doc_off = buf.len();
        let elem_count = bj.get_elem_count();
        let val_entry_start;
        if bj.type_code == JSONTypeCodeArray {
            let copy_size = headerSize + elem_count * valEntrySize;
            val_entry_start = headerSize;
            buf.extend_from_slice(&bj.value[..copy_size]);
        } else {
            let copy_size = headerSize + elem_count * (keyEntrySize + valEntrySize);
            val_entry_start = headerSize + elem_count * keyEntrySize;
            buf.extend_from_slice(&bj.value[..copy_size]);
            if elem_count > 0 {
                let first_key_off = le_u32(&bj.value[headerSize..]) as usize;
                let last_key_off =
                    le_u32(&bj.value[headerSize + (elem_count - 1) * keyEntrySize..]) as usize;
                let last_key_len =
                    le_u16(&bj.value[headerSize + (elem_count - 1) * keyEntrySize + keyLenOff..])
                        as usize;
                buf.extend_from_slice(&bj.value[first_key_off..last_key_off + last_key_len]);
            }
        }
        for i in 0..elem_count {
            let val_entry_off = val_entry_start + i * valEntrySize;
            let elem = bj.val_entry_get(val_entry_off);
            let val_off = buf.len() - doc_off;
            let tp_code = self.rebuild_to(buf, elem);
            buf[doc_off + val_entry_off] = tp_code;
            if tp_code == JSONTypeCodeLiteral {
                let last_idx = buf.len() - 1;
                let lit = u32::from(buf[last_idx]);
                put_u32(&mut buf[doc_off + val_entry_off + valTypeSize..], lit);
                buf.truncate(last_idx);
            } else {
                put_u32(
                    &mut buf[doc_off + val_entry_off + valTypeSize..],
                    val_off as u32,
                );
            }
        }
        let size = (buf.len() - doc_off) as u32;
        put_u32(&mut buf[doc_off + dataSizeOff..], size);
        bj.type_code
    }
}

/// floatEpsilon is the acceptable error quantity when comparing two float numbers.
const floatEpsilon: f64 = 1.0e-8;

/// compareFloat64PrecisionLoss returns an integer comparing the float64 x to y,
/// allowing precision loss.
// go: compareFloat64PrecisionLoss L1
fn compare_float64_precision_loss(x: f64, y: f64) -> isize {
    if x - y < floatEpsilon && y - x < floatEpsilon {
        0
    } else if x - y < 0.0 {
        -1
    } else {
        1
    }
}

// go: compareInt64 L1
fn compare_int64(x: i64, y: i64) -> isize {
    if x < y {
        -1
    } else if x == y {
        0
    } else {
        1
    }
}

// go: compareFloat64 L1
fn compare_float64(x: f64, y: f64) -> isize {
    if x < y {
        -1
    } else if x == y {
        0
    } else {
        1
    }
}

// go: compareUint64 L1
fn compare_uint64(x: u64, y: u64) -> isize {
    if x < y {
        -1
    } else if x == y {
        0
    } else {
        1
    }
}

// go: compareInt64Uint64 L1
fn compare_int64_uint64(x: i64, y: u64) -> isize {
    if x < 0 {
        return -1;
    }
    compare_uint64(x as u64, y)
}

// go: compareFloat64Int64 L1
fn compare_float64_int64(x: f64, y: i64) -> isize {
    compare_float64_precision_loss(x, y as f64)
}

// go: compareFloat64Uint64 L1
fn compare_float64_uint64(x: f64, y: u64) -> isize {
    compare_float64_precision_loss(x, y as f64)
}

/// CompareBinaryJSON compares two binary json objects. Returns -1 if left < right,
/// 0 if left == right, else returns 1.
// go: CompareBinaryJSON L1
pub fn compare_binary_json(left: BinaryJSON<'_>, right: BinaryJSON<'_>) -> isize {
    let precedence1 = json_type_precedences(left.r#type());
    let precedence2 = json_type_precedences(right.r#type());
    let mut cmp: isize = 0;
    if precedence1 == precedence2 {
        if precedence1 == json_type_precedences("NULL") {
            // for JSON null.
            cmp = 0;
        }
        match left.type_code {
            JSONTypeCodeLiteral => {
                // false is less than true.
                cmp = right.value[0] as isize - left.value[0] as isize;
            }
            JSONTypeCodeInt64 => match right.type_code {
                JSONTypeCodeInt64 => cmp = compare_int64(left.get_int64(), right.get_int64()),
                JSONTypeCodeUint64 => {
                    cmp = compare_int64_uint64(left.get_int64(), right.get_uint64())
                }
                JSONTypeCodeFloat64 => {
                    cmp = -compare_float64_int64(right.get_float64(), left.get_int64());
                }
                _ => {}
            },
            JSONTypeCodeUint64 => match right.type_code {
                JSONTypeCodeInt64 => {
                    cmp = -compare_int64_uint64(right.get_int64(), left.get_uint64());
                }
                JSONTypeCodeUint64 => cmp = compare_uint64(left.get_uint64(), right.get_uint64()),
                JSONTypeCodeFloat64 => {
                    cmp = -compare_float64_uint64(right.get_float64(), left.get_uint64());
                }
                _ => {}
            },
            JSONTypeCodeFloat64 => match right.type_code {
                JSONTypeCodeInt64 => {
                    cmp = compare_float64_int64(left.get_float64(), right.get_int64())
                }
                JSONTypeCodeUint64 => {
                    cmp = compare_float64_uint64(left.get_float64(), right.get_uint64())
                }
                JSONTypeCodeFloat64 => {
                    cmp = compare_float64(left.get_float64(), right.get_float64())
                }
                _ => {}
            },
            JSONTypeCodeString => cmp = bytes_compare(left.get_string(), right.get_string()),
            JSONTypeCodeArray => {
                let left_count = left.get_elem_count();
                let right_count = right.get_elem_count();
                let mut i = 0;
                while i < left_count && i < right_count {
                    let elem1 = left.array_get_elem(i);
                    let elem2 = right.array_get_elem(i);
                    cmp = compare_binary_json(elem1, elem2);
                    if cmp != 0 {
                        return cmp;
                    }
                    i += 1;
                }
                cmp = left_count as isize - right_count as isize;
            }
            JSONTypeCodeObject => {
                // reference:
                // https://github.com/mysql/mysql-server/blob/ee4455a33b10f1b1886044322e4893f587b319ed/sql/json_dom.cc#L2561
                let (left_count, right_count) = (left.get_elem_count(), right.get_elem_count());
                let cmp = compare_int64(left_count as i64, right_count as i64);
                if cmp != 0 {
                    return cmp;
                }
                for i in 0..left_count {
                    let (left_key, right_key) = (left.object_get_key(i), right.object_get_key(i));
                    let cmp = bytes_compare(left_key, right_key);
                    if cmp != 0 {
                        return cmp;
                    }
                    let cmp = compare_binary_json(left.object_get_val(i), right.object_get_val(i));
                    if cmp != 0 {
                        return cmp;
                    }
                }
                // Go's inner `cmp :=` shadows the outer one, which stays 0 here.
                return 0;
            }
            JSONTypeCodeOpaque => {
                cmp = bytes_compare(&left.get_opaque().buf, &right.get_opaque().buf);
            }
            JSONTypeCodeDate | JSONTypeCodeDatetime | JSONTypeCodeTimestamp => {
                // the json_type_precedences guarantees that the DATE is only
                // comparable with the DATE, and the DATETIME and TIMESTAMP will compare with each other
                // as the `Type()` of `JSONTypeCodeTimestamp` is also `DATETIME`.
                let left_time = left.get_time();
                let right_time = right.get_time();
                cmp = left_time.compare(&right_time);
            }
            JSONTypeCodeDuration => {
                let left_duration = left.get_duration();
                let right_duration = right.get_duration();
                cmp = left_duration.compare(right_duration);
            }
            _ => {}
        }
    } else {
        cmp = precedence1 - precedence2;
        if cmp > 0 {
            cmp = 1;
        } else if cmp < 0 {
            cmp = -1;
        }
    }
    cmp
}

// go-std: bytes.Compare L1
fn bytes_compare(a: &[u8], b: &[u8]) -> isize {
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// MergePatchBinaryJSON implements RFC7396
/// https://datatracker.ietf.org/doc/html/rfc7396
///
/// `None` is Go's nil `*BinaryJSON`.
// go: MergePatchBinaryJSON L1 [M-ERR]
pub fn merge_patch_binary_json(
    bjs: &[Option<BinaryJSON<'_>>],
) -> Result<Option<BinaryJSON<'static>>, Error> {
    let length = bjs.len();
    let mut bjs = bjs;

    // according to the implements of RFC7396
    // when the last item is not object
    // we can return the last item directly
    for i in (0..length).rev() {
        if bjs[i]
            .as_ref()
            .is_none_or(|b| b.type_code != JSONTypeCodeObject)
        {
            bjs = &bjs[i..];
            break;
        }
    }

    let mut target: Option<BinaryJSON<'static>> = bjs[0].as_ref().map(|b| b.copy());
    for patch in &bjs[1..] {
        target = merge_patch_binary_json_inner(target.as_ref(), patch.as_ref())?;
    }
    Ok(target)
}

// go: mergePatchBinaryJSON L1 [M-ERR]
fn merge_patch_binary_json_inner(
    target: Option<&BinaryJSON<'_>>,
    patch: Option<&BinaryJSON<'_>>,
) -> Result<Option<BinaryJSON<'static>>, Error> {
    let Some(patch) = patch else {
        return Ok(None);
    };

    if patch.type_code == JSONTypeCodeObject {
        let Some(target) = target else {
            return Ok(None);
        };

        let mut key_val_map: HashMap<Vec<u8>, BinaryJSON<'static>> = HashMap::new();
        if target.type_code == JSONTypeCodeObject {
            let elem_count = target.get_elem_count();
            for i in 0..elem_count {
                let key = target.object_get_key(i);
                let val = target.object_get_val(i);
                key_val_map.insert(key.to_vec(), val.copy());
            }
        }
        let elem_count = patch.get_elem_count();
        for i in 0..elem_count {
            let key = patch.object_get_key(i);
            let val = patch.object_get_val(i);
            let k = key.to_vec();

            // A missing key is Go's zero BinaryJSON (TypeCode 0, nil Value), not nil.
            let target_kv = key_val_map.get(&k).cloned();
            let exists = target_kv.is_some();
            if val.type_code == JSONTypeCodeLiteral && val.value[0] == JSONLiteralNil {
                if exists {
                    key_val_map.remove(&k);
                }
            } else {
                let target_kv = target_kv.unwrap_or_default();
                let tmp = merge_patch_binary_json_inner(Some(&target_kv), Some(&val))?;
                key_val_map.insert(k, tmp.unwrap_or_default());
            }
        }

        let mut keys: Vec<Vec<u8>> = key_val_map.keys().cloned().collect();
        keys.sort();
        let values: Vec<BinaryJSON<'_>> = keys.iter().map(|k| key_val_map[k].as_ref()).collect();
        let key_refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();

        let binary_object = build_binary_json_object(&key_refs, &values)?;
        return Ok(Some(binary_object));
    }
    Ok(Some(patch.copy()))
}

/// MergeBinaryJSON merges multiple BinaryJSON into one according the following rules:
/// 1) adjacent arrays are merged to a single array;
/// 2) adjacent object are merged to a single object;
/// 3) a scalar value is autowrapped as an array before merge;
/// 4) an adjacent array and object are merged by autowrapping the object as an array.
// go: MergeBinaryJSON L1
pub fn merge_binary_json(bjs: &[BinaryJSON<'_>]) -> BinaryJSON<'static> {
    let mut remain = bjs;
    let mut results: Vec<BinaryJSON<'static>> = Vec::new();
    while !remain.is_empty() {
        if remain[0].type_code != JSONTypeCodeObject {
            results.push(remain[0].copy());
            remain = &remain[1..];
        } else {
            let objects;
            (objects, remain) = get_adjacent_objects(remain);
            results.push(merge_binary_object(objects));
        }
    }
    if results.len() == 1 {
        return results.remove(0);
    }
    merge_binary_array(&results)
}

// go: getAdjacentObjects L1
fn get_adjacent_objects<'s, 'a>(
    bjs: &'s [BinaryJSON<'a>],
) -> (&'s [BinaryJSON<'a>], &'s [BinaryJSON<'a>]) {
    for i in 0..bjs.len() {
        if bjs[i].type_code != JSONTypeCodeObject {
            return (&bjs[..i], &bjs[i..]);
        }
    }
    (bjs, &[])
}

// go: mergeBinaryArray L1
fn merge_binary_array(elems: &[BinaryJSON<'_>]) -> BinaryJSON<'static> {
    let mut buf: Vec<BinaryJSON<'_>> = Vec::with_capacity(elems.len());
    for elem in elems {
        if elem.type_code != JSONTypeCodeArray {
            buf.push(elem.as_ref());
        } else {
            let child_count = elem.get_elem_count();
            for j in 0..child_count {
                buf.push(elem.array_get_elem(j));
            }
        }
    }
    build_binary_json_array(&buf)
}

// go: mergeBinaryObject L1
fn merge_binary_object(objects: &[BinaryJSON<'_>]) -> BinaryJSON<'static> {
    let mut key_val_map: HashMap<Vec<u8>, BinaryJSON<'static>> = HashMap::new();
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for obj in objects {
        let elem_count = obj.get_elem_count();
        for i in 0..elem_count {
            let key = obj.object_get_key(i);
            let val = obj.object_get_val(i);
            if let Some(old) = key_val_map.get(key) {
                let merged = merge_binary_json(&[old.as_ref(), val]);
                key_val_map.insert(key.to_vec(), merged);
            } else {
                key_val_map.insert(key.to_vec(), val.copy());
                keys.push(key.to_vec());
            }
        }
    }
    keys.sort();
    let values: Vec<BinaryJSON<'_>> = keys.iter().map(|k| key_val_map[k].as_ref()).collect();
    let key_refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
    match build_binary_json_object(&key_refs, &values) {
        Ok(o) => o,
        Err(_) => {
            panic!("mergeBinaryObject should never panic, please contact the TiDB team for help")
        }
    }
}

/// PeekBytesAsJSON trys to peek some bytes from b, until
/// we can deserialize a JSON from those bytes.
// go: PeekBytesAsJSON L1 [M-ERRVAL]
pub fn peek_bytes_as_json(b: &[u8]) -> Result<isize, Error> {
    if b.is_empty() {
        return Err(Error::plain("Cant peek from empty bytes"));
    }
    match b[0] {
        JSONTypeCodeObject | JSONTypeCodeArray => {
            if b.len() >= valTypeSize + headerSize {
                let size = le_u32(&b[valTypeSize + dataSizeOff..]);
                return Ok(valTypeSize as isize + size as isize);
            }
        }
        JSONTypeCodeString => {
            let (str_len, len_len) = uvarint(&b[valTypeSize..]);
            return Ok(valTypeSize as isize + str_len as isize + len_len);
        }
        JSONTypeCodeInt64
        | JSONTypeCodeUint64
        | JSONTypeCodeFloat64
        | JSONTypeCodeDate
        | JSONTypeCodeDatetime
        | JSONTypeCodeTimestamp => return Ok(valTypeSize as isize + 8),
        JSONTypeCodeLiteral => return Ok(valTypeSize as isize + 1),
        JSONTypeCodeOpaque => {
            let (buf_len, len_len) = uvarint(&b[valTypeSize + 1..]);
            return Ok(valTypeSize as isize + 1 + buf_len as isize + len_len);
        }
        JSONTypeCodeDuration => return Ok(valTypeSize as isize + 12),
        _ => {}
    }
    Err(Error::plain("Invalid JSON bytes"))
}

/// ContainsBinaryJSON check whether JSON document contains specific target according the following rules:
/// 1) object contains a target object if and only if every key is contained in source object and the value associated with the target key is contained in the value associated with the source key;
/// 2) array contains a target nonarray if and only if the target is contained in some element of the array;
/// 3) array contains a target array if and only if every element is contained in some element of the array;
/// 4) scalar contains a target scalar if and only if they are comparable and are equal;
// go: ContainsBinaryJSON L1
pub fn contains_binary_json(obj: BinaryJSON<'_>, target: BinaryJSON<'_>) -> bool {
    match obj.type_code {
        JSONTypeCodeObject => {
            if target.type_code == JSONTypeCodeObject {
                let elem_count = target.get_elem_count();
                for i in 0..elem_count {
                    let key = target.object_get_key(i);
                    let val = target.object_get_val(i);
                    match obj.object_search_key(key) {
                        Some(exp) if contains_binary_json(exp.as_ref(), val.as_ref()) => {}
                        _ => return false,
                    }
                }
                return true;
            }
            false
        }
        JSONTypeCodeArray => {
            if target.type_code == JSONTypeCodeArray {
                let elem_count = target.get_elem_count();
                for i in 0..elem_count {
                    if !contains_binary_json(obj.as_ref(), target.array_get_elem(i)) {
                        return false;
                    }
                }
                return true;
            }
            let elem_count = obj.get_elem_count();
            for i in 0..elem_count {
                if contains_binary_json(obj.array_get_elem(i), target.as_ref()) {
                    return true;
                }
            }
            false
        }
        _ => compare_binary_json(obj, target) == 0,
    }
}

/// OverlapsBinaryJSON is similar with ContainsBinaryJSON, but it checks the `OR` relationship.
// go: OverlapsBinaryJSON L1
pub fn overlaps_binary_json(obj: BinaryJSON<'_>, target: BinaryJSON<'_>) -> bool {
    let (obj, target) =
        if obj.type_code != JSONTypeCodeArray && target.type_code == JSONTypeCodeArray {
            (target, obj)
        } else {
            (obj, target)
        };
    match obj.type_code {
        JSONTypeCodeObject => {
            if target.type_code == JSONTypeCodeObject {
                let elem_count = target.get_elem_count();
                for i in 0..elem_count {
                    let key = target.object_get_key(i);
                    let val = target.object_get_val(i);
                    if let Some(exp) = obj.object_search_key(key)
                        && compare_binary_json(exp, val) == 0
                    {
                        return true;
                    }
                }
            }
            false
        }
        JSONTypeCodeArray => {
            if target.type_code == JSONTypeCodeArray {
                for i in 0..obj.get_elem_count() {
                    let o = obj.array_get_elem(i);
                    for j in 0..target.get_elem_count() {
                        if compare_binary_json(o.as_ref(), target.array_get_elem(j)) == 0 {
                            return true;
                        }
                    }
                }
                return false;
            }
            let elem_count = obj.get_elem_count();
            for i in 0..elem_count {
                if compare_binary_json(obj.array_get_elem(i), target.as_ref()) == 0 {
                    return true;
                }
            }
            false
        }
        _ => compare_binary_json(obj, target) == 0,
    }
}
