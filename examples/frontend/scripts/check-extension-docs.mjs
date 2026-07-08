#!/usr/bin/env node
/**
 * Enforces file-level and top-level type/function documentation for extension TypeScript sources.
 */
import { readdir, readFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(scriptDir, "..");
const sourceSets = [
    {
        dir: join(projectRoot, "extension-assets"),
        extensions: [".ts"],
    },
    {
        dir: join(projectRoot, "scripts"),
        extensions: [".mts"],
    },
];
const sourceFiles = await collectSourceFiles(sourceSets);
const missingDocs = (await Promise.all(sourceFiles.map(async (file) => checkFile(file)))).flat();
if (missingDocs.length > 0) {
    for (const missing of missingDocs) {
        console.error(`${missing.file}:${missing.line}:${missing.column}: missing JSDoc for ${missing.declaration}`);
    }
    process.exitCode = 1;
}
/**
 * Collects source files from configured directories without adding a glob dependency.
 */
async function collectSourceFiles(sourceSetList) {
    const files = [];
    for (const sourceSet of sourceSetList) {
        const entries = await readdir(sourceSet.dir);
        for (const entry of entries) {
            if (sourceSet.extensions.includes(extname(entry))) {
                files.push(join(sourceSet.dir, entry));
            }
        }
    }
    return files.sort();
}
/**
 * Parses one TypeScript source file and returns all missing doc diagnostics.
 */
async function checkFile(file) {
    const text = await readFile(file, "utf8");
    const sourceFile = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
    const diagnostics = [];
    const [firstStatement] = sourceFile.statements;
    if (!firstStatement || !hasLeadingJsDoc(firstStatement, sourceFile)) {
        diagnostics.push(diagnostic(file, sourceFile, firstStatement ?? sourceFile, "file"));
    }
    for (const statement of sourceFile.statements) {
        const declarationName = documentedDeclarationName(statement);
        if (!declarationName) {
            continue;
        }
        if (!hasLeadingJsDoc(statement, sourceFile)) {
            diagnostics.push(diagnostic(file, sourceFile, statement, declarationName));
        }
    }
    return diagnostics;
}
/**
 * Returns the name of a top-level declaration covered by the doc gate.
 */
function documentedDeclarationName(node) {
    if (ts.isFunctionDeclaration(node)) {
        return node.name?.text ? `function ${node.name.text}` : "anonymous function";
    }
    if (ts.isTypeAliasDeclaration(node)) {
        return `type ${node.name.text}`;
    }
    if (ts.isInterfaceDeclaration(node)) {
        return `interface ${node.name.text}`;
    }
    if (ts.isClassDeclaration(node)) {
        return node.name?.text ? `class ${node.name.text}` : "anonymous class";
    }
    if (ts.isEnumDeclaration(node)) {
        return `enum ${node.name.text}`;
    }
    return undefined;
}
/**
 * Checks whether a declaration has a leading block JSDoc comment.
 */
function hasLeadingJsDoc(node, sourceFile) {
    const ranges = ts.getLeadingCommentRanges(sourceFile.text, node.getFullStart()) ?? [];
    return ranges.some((range) => {
        const comment = sourceFile.text.slice(range.pos, range.end).trimStart();
        return comment.startsWith("/**");
    });
}
/**
 * Converts a source node into a user-facing missing documentation diagnostic.
 */
function diagnostic(file, sourceFile, node, declaration) {
    const position = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
    return {
        file,
        line: position.line + 1,
        column: position.character + 1,
        declaration,
    };
}
