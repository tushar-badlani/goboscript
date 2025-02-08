use core::str;
use std::{
    fs::File,
    io::{self, Seek, Write},
    path::Path,
};

use fxhash::{FxHashMap, FxHashSet};
use logos::Span;
use md5::{Digest, Md5};
use serde_json::json;
use zip::{write::SimpleFileOptions, ZipWriter};

use super::{
    cmd::cmd_to_list, node::Node, node_id::NodeID, node_id_factory::NodeIDFactory,
    turbowarp_config::TurbowarpConfig,
};
use crate::{
    ast::*,
    blocks::Block,
    codegen::mutation::Mutation,
    config::Config,
    diagnostic::{DiagnosticKind, SpriteDiagnostics},
    misc::{write_comma_io, SmolStr},
};

const STAGE_NAME: &str = "Stage";

#[derive(Debug, Copy, Clone)]
pub struct S<'a> {
    pub stage: Option<&'a Sprite>,
    pub sprite: &'a Sprite,
    pub proc: Option<&'a Proc>,
    pub func: Option<&'a Func>,
}

pub type D<'a> = &'a mut SpriteDiagnostics;

pub enum QualifiedName {
    Var(SmolStr, Type),
    List(SmolStr, Type),
}

pub fn qualify_local_var_name(proc_name: &str, var_name: &str) -> SmolStr {
    format!("{}:{}", proc_name, var_name).into()
}

pub fn qualify_struct_var_name(field_name: &str, var_name: &str) -> SmolStr {
    format!("{}.{}", var_name, field_name).into()
}

impl S<'_> {
    pub fn is_name_list(&self, name: &Name) -> bool {
        self.sprite.lists.contains_key(name.basename())
            || self
                .stage
                .is_some_and(|stage| stage.lists.contains_key(name.basename()))
    }

    fn get_local_var(&self, name: &str) -> Option<&Var> {
        self.proc
            .and_then(|proc| proc.locals.get(name))
            .or_else(|| self.func.and_then(|func| func.locals.get(name)))
    }

    fn get_var(&self, name: &str) -> Option<&Var> {
        self.sprite
            .vars
            .get(name)
            .or_else(|| self.stage.and_then(|stage| stage.vars.get(name)))
    }

    pub fn get_list(&self, name: &str) -> Option<&List> {
        self.sprite
            .lists
            .get(name)
            .or_else(|| self.stage.and_then(|stage| stage.lists.get(name)))
    }

    pub fn get_struct(&self, name: &str) -> Option<&Struct> {
        self.sprite
            .structs
            .get(name)
            .or_else(|| self.stage.and_then(|stage| stage.structs.get(name)))
    }

    pub fn get_enum(&self, name: &str) -> Option<&Enum> {
        self.sprite
            .enums
            .get(name)
            .or_else(|| self.stage.and_then(|stage| stage.enums.get(name)))
    }

    fn qualify_field<T>(
        &self,
        d: D,
        span: &Span,
        qualified_var_name: SmolStr,
        field_name: Option<SmolStr>,
        type_: &Type,
        variant: T,
    ) -> Option<QualifiedName>
    where
        T: FnOnce(SmolStr, Type) -> QualifiedName,
    {
        match type_ {
            Type::Value => match field_name {
                None => Some(variant(qualified_var_name, type_.clone())),
                Some(_) => {
                    d.report(DiagnosticKind::NotStruct, span);
                    None
                }
            },
            Type::Struct {
                name: type_name,
                span: type_span,
            } => match field_name {
                None => {
                    eprintln!("attempted to qualify field without field name: {qualified_var_name} with type: {type_}");
                    None
                }
                Some(field_name) => {
                    let struct_ = self.get_struct(type_name)?;
                    if !struct_.fields.iter().any(|field| field.name == field_name) {
                        d.report(
                            DiagnosticKind::StructDoesNotHaveField {
                                type_name: type_name.clone(),
                                field_name: field_name.clone(),
                            },
                            type_span,
                        );
                        None
                    } else {
                        Some(variant(
                            qualify_struct_var_name(&field_name, &qualified_var_name),
                            type_.clone(),
                        ))
                    }
                }
            },
        }
    }

    pub fn qualify_name(&self, d: D, name: &Name) -> Option<QualifiedName> {
        let basename = name.basename();
        let fieldname = name.fieldname().cloned();
        if let Some(list) = self.get_list(basename) {
            return self.qualify_field(
                d,
                &name.span(),
                list.name.clone(),
                fieldname,
                &list.type_,
                QualifiedName::List,
            );
        }
        if let Some(var) = self.get_local_var(basename) {
            let qualified_var_name = qualify_local_var_name(
                self.proc
                    .map(|proc| &proc.name)
                    .unwrap_or_else(|| &self.func.unwrap().name),
                &var.name,
            );
            return self.qualify_field(
                d,
                &name.span(),
                qualified_var_name,
                fieldname,
                &var.type_,
                QualifiedName::Var,
            );
        }
        if let Some(var) = self.get_var(basename) {
            return self.qualify_field(
                d,
                &name.span(),
                var.name.clone(),
                fieldname,
                &var.type_,
                QualifiedName::Var,
            );
        }
        d.report(
            DiagnosticKind::UnrecognizedVariable(basename.clone()),
            &name.span(),
        );
        None
    }
}

impl Stmt {
    fn is_terminator(&self) -> bool {
        matches!(
            self,
            Stmt::Forever { .. }
                | Stmt::Block {
                    block: Block::DeleteThisClone | Block::StopAll | Block::StopThisScript,
                    ..
                }
        )
    }
    fn opcode(&self, s: S) -> &'static str {
        match self {
            Stmt::Repeat { .. } => "control_repeat",
            Stmt::Forever { .. } => "control_forever",
            Stmt::Branch { else_body, .. } => {
                if else_body.is_empty() {
                    "control_if"
                } else {
                    "control_if_else"
                }
            }
            Stmt::Until { .. } => "control_repeat_until",
            Stmt::SetVar { .. } => "data_setvariableto",
            Stmt::ChangeVar { .. } => "data_changevariableby",
            Stmt::Show(name) => {
                if s.is_name_list(name) {
                    "data_showlist"
                } else {
                    "data_showvariable"
                }
            }
            Stmt::Hide(name) => {
                if s.is_name_list(name) {
                    "data_hidelist"
                } else {
                    "data_hidevariable"
                }
            }
            Stmt::AddToList { .. } => "data_addtolist",
            Stmt::DeleteListIndex { .. } => "data_deleteoflist",
            Stmt::DeleteList { .. } => "data_deletealloflist",
            Stmt::InsertAtList { .. } => "data_insertatlist",
            Stmt::SetListIndex { .. } => "data_replaceitemoflist",
            Stmt::Block { block, .. } => block.opcode(),
            Stmt::ProcCall { .. } => "procedures_call",
            Stmt::FuncCall { .. } => "procedures_call",
            Stmt::Return { .. } => "data_setvariableto",
        }
    }
}

#[derive(Debug)]
pub struct Sb3<T>
where
    T: Write + Seek,
{
    pub zip: ZipWriter<T>,
    pub id: NodeIDFactory,
    pub node_comma: bool,
    pub inputs_comma: bool,
    pub costumes: FxHashMap<SmolStr, SmolStr>,
    pub srcpkg_hash: Option<String>,
    pub srcpkg: Option<Vec<u8>>,
}

impl<T> Write for Sb3<T>
where
    T: Write + Seek,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.zip.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.zip.flush()
    }
}

impl<T> Sb3<T>
where
    T: Write + Seek,
{
    pub fn new(file: T) -> Self {
        Self {
            zip: ZipWriter::new(file),
            id: NodeIDFactory::new(),
            node_comma: false,
            inputs_comma: false,
            costumes: FxHashMap::default(),
            srcpkg_hash: None,
            srcpkg: None,
        }
    }

    fn assets(&mut self, input: &Path) -> io::Result<()> {
        let mut added = FxHashSet::default();
        for (path, hash) in &self.costumes {
            if added.contains(hash) {
                continue;
            }
            added.insert(hash);
            let (_, extension) = path.rsplit_once('.').unwrap();
            self.zip
                .start_file(format!("{hash}.{extension}"), SimpleFileOptions::default())?;
            let file = File::open(input.join(&**path));
            io::copy(&mut file?, &mut self.zip)?;
        }
        if self.srcpkg_hash.is_some() {
            let hash = self.srcpkg_hash.take().unwrap();
            let data = self.srcpkg.take().unwrap();
            self.zip
                .start_file(format!("{hash}.svg"), SimpleFileOptions::default())?;
            self.zip.write_all(&data)?;
        }
        Ok(())
    }

    pub fn begin_node(&mut self, node: Node) -> io::Result<()> {
        write_comma_io(&mut self.zip, &mut self.node_comma)?;
        write!(self, "{node}")
    }

    pub fn end_obj(&mut self) -> io::Result<()> {
        self.write_all(b"}")
    }

    pub fn begin_inputs(&mut self) -> io::Result<()> {
        self.inputs_comma = false;
        self.write_all(br#","inputs":{"#)
    }

    pub fn single_field(&mut self, name: &'static str, value: &str) -> io::Result<()> {
        write!(self, r#","fields":{{"{name}":[{},null]}}"#, json!(value))
    }

    pub fn single_field_id(&mut self, name: &'static str, value: &str) -> io::Result<()> {
        write!(
            self,
            r#","fields":{{"{name}":[{},{}]}}"#,
            json!(value),
            json!(value)
        )
    }

    pub fn substack(&mut self, name: &str, this_id: Option<NodeID>) -> io::Result<()> {
        let Some(this_id) = this_id else {
            return Ok(());
        };
        write_comma_io(&mut self.zip, &mut self.inputs_comma)?;
        write!(self, r#""{name}":[2,{this_id}]"#)
    }

    pub fn project(
        &mut self,
        input: &Path,
        project: &Project,
        config: &Config,
        stage_diagnostics: D,
        sprites_diagnostics: &mut FxHashMap<SmolStr, SpriteDiagnostics>,
    ) -> io::Result<()> {
        let broadcasts: FxHashSet<_> = project
            .stage
            .events
            .iter()
            .chain(project.sprites.values().flat_map(|sprite| &sprite.events))
            .filter_map(|event| {
                if let EventKind::On { event } = &event.kind {
                    Some(event.clone())
                } else {
                    None
                }
            })
            .collect();
        // TODO: switch to deflate compression
        // this should be configurable, use store in debug (because it would be
        // faster?), use deflate in release (because it would be smaller?)
        self.zip
            .start_file("project.json", SimpleFileOptions::default())?;
        write!(self, "{{")?;
        write!(self, r#""targets":["#)?;
        self.sprite(
            input,
            STAGE_NAME,
            &project.stage,
            None,
            config,
            stage_diagnostics,
            Some(broadcasts),
        )?;
        for (sprite_name, sprite) in &project.sprites {
            write!(self, r#","#)?;
            self.sprite(
                input,
                sprite_name,
                sprite,
                Some(&project.stage),
                config,
                sprites_diagnostics.get_mut(sprite_name).unwrap(),
                None,
            )?;
        }
        write!(self, "]")?; // targets
        write!(self, r#","monitors":[]"#)?;
        write!(self, r#","extensions":[]"#)?;
        write!(self, r#","meta":{{"#)?;
        write!(self, r#""semver":"3.0.0""#)?;
        write!(self, r#","vm":"0.2.0""#)?;
        write!(
            self,
            r#","agent":"goboscript v{}""#,
            env!("CARGO_PKG_VERSION")
        )?;
        write!(self, "}}")?; // meta
        write!(self, "}}")?; // project
        self.assets(input)?;
        Ok(())
    }

    pub fn sprite(
        &mut self,
        input: &Path,
        name: &str,
        sprite: &Sprite,
        stage: Option<&Sprite>,
        config: &Config,
        d: D,
        broadcasts: Option<FxHashSet<SmolStr>>,
    ) -> io::Result<()> {
        for proc in sprite.procs.values() {
            if !sprite.used_procs.contains(&proc.name) {
                d.report(DiagnosticKind::UnusedProc(proc.name.clone()), &proc.span);
            } else {
                for arg in &proc.args {
                    if !sprite
                        .proc_used_args
                        .get(&proc.name)
                        .unwrap()
                        .contains(&arg.name)
                    {
                        d.report(DiagnosticKind::UnusedArg(arg.name.clone()), &arg.span);
                    }
                }
            }
        }
        for func in sprite.funcs.values() {
            if !sprite.used_funcs.contains(&func.name) {
                d.report(DiagnosticKind::UnusedFunc(func.name.clone()), &func.span);
            } else {
                for arg in &func.args {
                    if !sprite
                        .func_used_args
                        .get(&func.name)
                        .unwrap()
                        .contains(&arg.name)
                    {
                        d.report(DiagnosticKind::UnusedArg(arg.name.clone()), &arg.span);
                    }
                }
            }
        }
        for struct_ in sprite.structs.values() {
            if !struct_.is_used {
                d.report(
                    DiagnosticKind::UnusedStruct(struct_.name.clone()),
                    &struct_.span,
                );
            }
        }
        for enum_ in sprite.enums.values() {
            if !enum_.is_used {
                d.report(DiagnosticKind::UnusedEnum(enum_.name.clone()), &enum_.span);
            }
        }
        self.id.reset();
        write!(self, "{{")?;
        write!(self, r#""isStage":{}"#, name == STAGE_NAME)?;
        write!(self, r#","name":{}"#, json!(name))?;
        if name == STAGE_NAME {
            write!(self, r#","comments":{{"#)?;
            write!(self, r#""twconfig":{{"#)?;
            write!(self, r#""blockId":null"#)?;
            write!(self, r#","x":0"#)?;
            write!(self, r#","y":0"#)?;
            write!(self, r#","width":350"#)?;
            write!(self, r#","height":170"#)?;
            write!(self, r#","minimized":false"#)?;
            write!(
                self,
                r#","text":{}"#,
                json!(TurbowarpConfig::from(config).to_string())
            )?;
            write!(self, "}}")?; // twconfig
            write!(self, "}}")?; // comments
        }
        write!(self, r#","broadcasts":{{"#)?;
        let mut comma = false;
        for broadcast in broadcasts.unwrap_or_default() {
            write_comma_io(&mut self.zip, &mut comma)?;
            write!(self, r#"{}:{}"#, json!(*broadcast), json!(*broadcast))?;
        }
        write!(self, "}}")?; // broadcasts
        write!(self, r#","variables":{{"#)?;
        let mut comma = false;
        for proc in sprite
            .procs
            .values()
            .filter(|proc| sprite.used_procs.contains(&proc.name))
        {
            for var in proc.locals.values() {
                self.local_var_declaration(sprite, &proc.name, var, &mut comma, d)?;
            }
        }
        for func in sprite
            .funcs
            .values()
            .filter(|func| sprite.used_funcs.contains(&func.name))
        {
            for var in func.locals.values() {
                self.local_var_declaration(sprite, &func.name, var, &mut comma, d)?;
            }
        }
        for var in sprite.vars.values().filter(|var| var.is_used) {
            self.var_declaration(sprite, var, &mut comma, d)?;
        }
        write!(self, "}}")?; // variables
        write!(self, r#","lists":{{"#)?;
        let mut comma = false;
        for list in sprite.lists.values().filter(|list| list.is_used) {
            self.list_declaration(input, sprite, list, &mut comma, d)?;
        }
        write!(self, "}}")?; // lists
        write!(self, r#","blocks":{{"#)?;
        self.node_comma = false;
        for proc in sprite
            .procs
            .values()
            .filter(|proc| sprite.used_procs.contains(&proc.name))
        {
            let proc_definition = sprite.proc_definitions.get(&proc.name).unwrap();
            self.proc(
                S {
                    stage,
                    sprite,
                    proc: Some(proc),
                    func: None,
                },
                d,
                proc,
                proc_definition,
            )?;
        }
        for func in sprite
            .funcs
            .values()
            .filter(|func| sprite.used_funcs.contains(&func.name))
        {
            let func_definition = sprite.func_definitions.get(&func.name).unwrap();
            self.func(
                S {
                    stage,
                    sprite,
                    proc: None,
                    func: Some(func),
                },
                d,
                func,
                func_definition,
            )?;
        }
        for event in &sprite.events {
            self.event(
                S {
                    stage,
                    sprite,
                    proc: None,
                    func: None,
                },
                d,
                event,
            )?;
        }
        write!(self, "}}")?; // blocks
        if sprite.costumes.is_empty() {
            d.report(DiagnosticKind::NoCostumes, &(0..0));
        }
        write!(self, r#","costumes":["#)?;
        let mut comma = false;
        for costume in &sprite.costumes {
            write_comma_io(&mut self.zip, &mut comma)?;
            self.costume(input, costume, d)?;
        }
        write!(self, "]")?; // costumes
        write!(self, r#","sounds":["#)?;
        write!(self, "]")?; // sounds
        write!(self, "}}")?; // sprite
        Ok(())
    }

    pub fn json_var_declaration(
        &mut self,
        var_name: &str,
        is_cloud: bool,
        comma: &mut bool,
    ) -> io::Result<()> {
        write_comma_io(&mut self.zip, comma)?;
        if is_cloud {
            write!(self, "\"{}\":[\"\u{2601} {}\",0,true]", var_name, var_name)
        } else {
            write!(self, "\"{}\":[\"{}\",0]", var_name, var_name)
        }
    }

    pub fn var_declaration(
        &mut self,
        sprite: &Sprite,
        var: &Var,
        comma: &mut bool,
        d: D,
    ) -> io::Result<()> {
        match &var.type_ {
            Type::Value => {
                self.json_var_declaration(&var.name, var.is_cloud, comma)?;
            }
            Type::Struct {
                name: type_name,
                span: type_span,
            } => {
                let Some(struct_) = sprite.structs.get(type_name) else {
                    d.report(
                        DiagnosticKind::UnrecognizedStruct(type_name.clone()),
                        type_span,
                    );
                    return Ok(());
                };
                for field in &struct_.fields {
                    let qualified_var_name = qualify_struct_var_name(&field.name, &var.name);
                    self.json_var_declaration(&qualified_var_name, false, comma)?;
                }
            }
        }
        Ok(())
    }

    pub fn local_var_declaration(
        &mut self,
        sprite: &Sprite,
        proc_name: &str,
        var: &Var,
        comma: &mut bool,
        d: D,
    ) -> io::Result<()> {
        match &var.type_ {
            Type::Value => {
                let qualified_var_name = qualify_local_var_name(proc_name, &var.name);
                self.json_var_declaration(&qualified_var_name, false, comma)?;
            }
            Type::Struct {
                name: type_name,
                span: type_span,
            } => {
                let Some(struct_) = sprite.structs.get(type_name) else {
                    d.report(
                        DiagnosticKind::UnrecognizedStruct(type_name.clone()),
                        type_span,
                    );
                    return Ok(());
                };
                for field in &struct_.fields {
                    let qualified_var_name = qualify_local_var_name(
                        proc_name,
                        &qualify_struct_var_name(&field.name, &var.name),
                    );
                    self.json_var_declaration(&qualified_var_name, false, comma)?;
                }
            }
        }
        Ok(())
    }

    pub fn list_declaration(
        &mut self,
        input: &Path,
        sprite: &Sprite,
        list: &List,
        comma: &mut bool,
        d: D,
    ) -> io::Result<()> {
        let data = list
            .cmd()
            .and_then(|cmd| {
                cmd_to_list(cmd, input)
                    .map_err(|err| d.diagnostics.push(err))
                    .ok()
            })
            .or_else(|| {
                list.array().map(|array| {
                    array
                        .iter()
                        .map(|const_expr| const_expr.evaluate().to_string())
                        .collect::<Vec<_>>()
                })
            });
        match &list.type_ {
            Type::Value => {
                write_comma_io(&mut self.zip, comma)?;
                if let Some(cmd) = data {
                    write!(self, r#""{}":["{}",{}]"#, list.name, list.name, json!(cmd))?;
                } else {
                    write!(self, r#""{}":["{}",[]]"#, list.name, list.name)?;
                }
            }
            Type::Struct {
                name: type_name,
                span: type_span,
            } => {
                let Some(struct_) = sprite.structs.get(type_name) else {
                    d.report(
                        DiagnosticKind::UnrecognizedStruct(type_name.clone()),
                        type_span,
                    );
                    return Ok(());
                };
                for (i, field) in struct_.fields.iter().enumerate() {
                    let qualified_list_name = qualify_struct_var_name(&field.name, &list.name);
                    write_comma_io(&mut self.zip, comma)?;
                    if let Some(cmd) = &data {
                        let column = (0..(cmd.len() / struct_.fields.len()))
                            .map(|j| &cmd[j * struct_.fields.len() + i])
                            .collect::<Vec<_>>();
                        write!(
                            self,
                            r#""{}":["{}",{}]"#,
                            qualified_list_name,
                            qualified_list_name,
                            json!(column)
                        )?;
                    } else {
                        write!(
                            self,
                            r#""{}":["{}",[]]"#,
                            qualified_list_name, qualified_list_name
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn costume(&mut self, input: &Path, costume: &Costume, d: D) -> io::Result<()> {
        let path = input.join(&*costume.path);
        let hash = self
            .costumes
            .get(&costume.path)
            .cloned()
            .map(Ok::<_, io::Error>)
            .unwrap_or_else(|| {
                let mut file = match File::open(&path) {
                    Ok(file) => file,
                    Err(error) => {
                        d.report(DiagnosticKind::IOError(error), &costume.span);
                        return Ok(Default::default());
                    }
                };
                let mut hasher = Md5::new();
                io::copy(&mut file, &mut hasher)?;
                let hash: SmolStr = format!("{:x}", hasher.finalize()).into();
                self.costumes.insert(costume.path.clone(), hash.clone());
                Ok(hash)
            })?;
        let (_, extension) = costume.path.rsplit_once('.').unwrap_or_default();
        self.costume_entry(&costume.name, &hash, extension)
    }

    pub fn costume_entry(&mut self, name: &str, hash: &str, extension: &str) -> io::Result<()> {
        write!(self, "{{")?;
        write!(self, r#""name":{}"#, json!(name))?;
        write!(self, r#","assetId":"{hash}""#)?;
        if extension == "png" || extension == "bmp" {
            write!(self, r#","bitmapResolution":1"#)?;
        }
        write!(self, r#","dataFormat":"{extension}""#)?;
        write!(self, r#","md5ext":"{hash}.{extension}""#)?;
        write!(self, "}}") // costume
    }

    pub fn proc(&mut self, s: S, d: D, proc: &Proc, definition: &[Stmt]) -> io::Result<()> {
        let this_id = self.id.new_id();
        let prototype_id = self.id.new_id();
        let next_id = self.id.new_id();
        self.begin_node(
            Node::new("procedures_definition", this_id)
                .some_next_id((!definition.is_empty()).then_some(next_id))
                .top_level(true),
        )?;
        self.begin_inputs()?;
        write!(self, r#""custom_block":[1,{prototype_id}]"#)?;
        self.end_obj()?; // inputs
        self.end_obj()?; // node
        let mut qualified_args: Vec<(SmolStr, NodeID)> = Vec::new();
        for arg in &proc.args {
            match &arg.type_ {
                Type::Value => {
                    let arg_id = self.id.new_id();
                    self.begin_node(
                        Node::new("argument_reporter_string_number", arg_id)
                            .parent_id(prototype_id)
                            .shadow(true),
                    )?;
                    self.single_field("VALUE", &arg.name)?;
                    self.end_obj()?; // node
                    qualified_args.push((arg.name.clone(), arg_id));
                }
                Type::Struct {
                    name: type_name,
                    span: type_span,
                } => {
                    let Some(struct_) = s.sprite.structs.get(type_name) else {
                        d.report(
                            DiagnosticKind::UnrecognizedStruct(type_name.clone()),
                            type_span,
                        );
                        continue;
                    };
                    for field in &struct_.fields {
                        let qualified_arg_name = qualify_struct_var_name(&field.name, &arg.name);
                        let arg_id = self.id.new_id();
                        self.begin_node(
                            Node::new("argument_reporter_string_number", arg_id)
                                .parent_id(prototype_id)
                                .shadow(true),
                        )?;
                        self.single_field("VALUE", &qualified_arg_name)?;
                        self.end_obj()?; // node
                        qualified_args.push((qualified_arg_name, arg_id));
                    }
                }
            }
        }
        self.begin_node(
            Node::new("procedures_prototype", prototype_id)
                .parent_id(this_id)
                .shadow(true),
        )?;
        self.begin_inputs()?;
        let mut comma = false;
        for (qualified_arg_name, arg_id) in &qualified_args {
            write_comma_io(&mut self.zip, &mut comma)?;
            write!(self, r#"{}:[2,{arg_id}]"#, json!(**qualified_arg_name))?;
        }
        self.end_obj()?; // inputs
        write!(
            self,
            "{}",
            Mutation::prototype(proc.name.clone(), &qualified_args, proc.warp)
        )?;
        self.end_obj()?; // node
        self.stmts(s, d, definition, next_id, Some(this_id))
    }

    pub fn func(&mut self, s: S, d: D, func: &Func, definition: &[Stmt]) -> io::Result<()> {
        let this_id = self.id.new_id();
        let prototype_id = self.id.new_id();
        let next_id = self.id.new_id();
        self.begin_node(
            Node::new("procedures_definition", this_id)
                .some_next_id((!definition.is_empty()).then_some(next_id))
                .top_level(true),
        )?;
        self.begin_inputs()?;
        write!(self, r#""custom_block":[1,{prototype_id}]"#)?;
        self.end_obj()?; // inputs
        self.end_obj()?; // node
        let mut qualified_args: Vec<(SmolStr, NodeID)> = Vec::new();
        for arg in &func.args {
            match &arg.type_ {
                Type::Value => {
                    let arg_id = self.id.new_id();
                    self.begin_node(
                        Node::new("argument_reporter_string_number", arg_id)
                            .parent_id(prototype_id)
                            .shadow(true),
                    )?;
                    self.single_field("VALUE", &arg.name)?;
                    self.end_obj()?; // node
                    qualified_args.push((arg.name.clone(), arg_id));
                }
                Type::Struct {
                    name: type_name,
                    span: type_span,
                } => {
                    let Some(struct_) = s.sprite.structs.get(type_name) else {
                        d.report(
                            DiagnosticKind::UnrecognizedStruct(type_name.clone()),
                            type_span,
                        );
                        continue;
                    };
                    for field in &struct_.fields {
                        let qualified_arg_name = qualify_struct_var_name(&field.name, &arg.name);
                        let arg_id = self.id.new_id();
                        self.begin_node(
                            Node::new("argument_reporter_string_number", arg_id)
                                .parent_id(prototype_id)
                                .shadow(true),
                        )?;
                        self.single_field("VALUE", &qualified_arg_name)?;
                        self.end_obj()?; // node
                        qualified_args.push((qualified_arg_name, arg_id));
                    }
                }
            }
        }
        self.begin_node(
            Node::new("procedures_prototype", prototype_id)
                .parent_id(this_id)
                .shadow(true),
        )?;
        self.begin_inputs()?;
        let mut comma = false;
        for (qualified_arg_name, arg_id) in &qualified_args {
            write_comma_io(&mut self.zip, &mut comma)?;
            write!(self, r#"{}:[2,{arg_id}]"#, json!(**qualified_arg_name))?;
        }
        self.end_obj()?; // inputs
        write!(
            self,
            "{}",
            Mutation::prototype(func.name.clone(), &qualified_args, true)
        )?;
        self.end_obj()?; // node
        self.stmts(s, d, definition, next_id, Some(this_id))
    }

    pub fn event(&mut self, s: S, d: D, event: &Event) -> io::Result<()> {
        let this_id = self.id.new_id();
        let next_id = self.id.new_id();
        self.begin_node(
            Node::new(event.kind.opcode(), this_id)
                .some_next_id((!event.body.is_empty()).then_some(next_id))
                .top_level(true),
        )?;
        match &event.kind {
            EventKind::On { event } => self.on(event),
            EventKind::OnFlag => self.on_flag(),
            EventKind::OnKey { key, span } => self.on_key(s, d, this_id, key, span),
            EventKind::OnClick => self.on_click(s, d, this_id),
            EventKind::OnBackdrop { backdrop, span } => {
                self.on_backdrop(s, d, this_id, backdrop, span)
            }
            EventKind::OnLoudnessGt { value } => self.on_loudness_gt(s, d, this_id, value),
            EventKind::OnTimerGt { value } => self.on_timer_gt(s, d, this_id, value),
            EventKind::OnClone => self.on_clone(s, d, this_id),
        }?;
        self.stmts(s, d, &event.body, next_id, Some(this_id))
    }

    pub fn stmts(
        &mut self,
        s: S,
        d: D,
        stmts: &[Stmt],
        mut this_id: NodeID,
        mut parent_id: Option<NodeID>,
    ) -> io::Result<()> {
        for (i, stmt) in stmts.iter().enumerate() {
            let is_last = i == stmts.len() - 1;
            if is_last || stmt.is_terminator() {
                self.stmt(s, d, stmt, this_id, None, parent_id)?;
                if !is_last {
                    d.report(DiagnosticKind::FollowedByUnreachableCode, stmt.span());
                }
                break;
            }
            let next_id = self.id.new_id();
            self.stmt(s, d, stmt, this_id, Some(next_id), parent_id)?;
            parent_id = Some(this_id);
            this_id = next_id;
        }
        Ok(())
    }

    pub fn stmt(
        &mut self,
        s: S,
        d: D,
        stmt: &Stmt,
        this_id: NodeID,
        next_id: Option<NodeID>,
        parent_id: Option<NodeID>,
    ) -> io::Result<()> {
        self.begin_node(
            Node::new(stmt.opcode(s), this_id)
                .some_next_id(next_id)
                .some_parent_id(parent_id),
        )?;
        match stmt {
            Stmt::Repeat { times, body } => self.repeat(s, d, this_id, times, body),
            Stmt::Forever { body, span } => self.forever(s, d, this_id, body, span),
            Stmt::Branch {
                cond,
                if_body,
                else_body,
            } => self.branch(s, d, this_id, cond, if_body, else_body),
            Stmt::Until { cond, body } => self.until(s, d, this_id, cond, body),
            Stmt::SetVar {
                name,
                value,
                type_,
                is_local,
                is_cloud,
            } => self.set_var(s, d, this_id, name, value, type_, is_local, is_cloud),
            Stmt::ChangeVar { name, value } => self.change_var(s, d, this_id, name, value),
            Stmt::Show(name) => self.show(s, d, name),
            Stmt::Hide(name) => self.hide(s, d, name),
            Stmt::AddToList { name, value } => self.add_to_list(s, d, this_id, name, value),
            Stmt::DeleteListIndex { name, index } => {
                self.delete_list_index(s, d, this_id, name, index)
            }
            Stmt::DeleteList(name) => self.delete_list(s, d, name),
            Stmt::InsertAtList { name, index, value } => {
                self.list_insert(s, d, this_id, name, index, value)
            }
            Stmt::SetListIndex { name, index, value } => {
                self.set_list_index(s, d, this_id, name, index, value)
            }
            Stmt::Block { block, span, args } => self.block(s, d, this_id, block, span, args),
            Stmt::ProcCall { name, span, args } => self.proc_call(s, d, this_id, name, span, args),
            Stmt::FuncCall { name, span, args } => self.func_call(s, d, this_id, name, span, args),
            Stmt::Return { .. } => panic!(),
        }
    }

    pub fn expr(
        &mut self,
        s: S,
        d: D,
        expr: &Expr,
        this_id: NodeID,
        parent_id: NodeID,
    ) -> io::Result<()> {
        match expr {
            Expr::Value { .. } => Ok(()),
            Expr::Name { .. } => Ok(()),
            Expr::Arg(name) => self.arg(s, d, this_id, parent_id, name),
            Expr::Repr { repr, span, args } => {
                self.repr(s, d, this_id, parent_id, repr, span, args)
            }
            Expr::FuncCall { name, span, .. } => {
                d.report(DiagnosticKind::UnrecognizedFunction(name.clone()), span);
                Ok(())
            }
            Expr::UnOp { op, span, opr } => self.un_op(s, d, this_id, parent_id, op, span, opr),
            Expr::BinOp { op, span, lhs, rhs } => {
                self.bin_op(s, d, this_id, parent_id, op, span, lhs, rhs)
            }
            Expr::StructLiteral { name, span, .. } => {
                d.report(
                    DiagnosticKind::TypeMismatch {
                        expected: Type::Value,
                        given: Type::Struct {
                            name: name.clone(),
                            span: span.clone(),
                        },
                    },
                    &expr.span(),
                );
                Ok(())
            }
            Expr::Dot { lhs, rhs, rhs_span } => {
                self.expr_dot(s, d, this_id, parent_id, lhs, rhs, rhs_span.clone())
            }
        }
    }
}
