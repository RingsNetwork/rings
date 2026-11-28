#!/usr/bin/env node

/**
 * Enforces file-level and top-level type/function documentation for extension TypeScript sources.
 */

import { readdir, readFile } from "node:fs/promises";
import { basename, dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

/**
 * A missing documentation diagnostic tied to one source location.
 */
type MissingDoc = {
  readonly file: string;
  readonly line: number;
  readonly column: number;
  readonly declaration: string;
};

/**
 * Describes a directory and file extension set that participates in the doc gate.
 */
type SourceSet = {
  readonly dir: string;
  readonly extensions: readonly string[];
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const projectRoot = frontendProjectRoot(scriptDir);
const sourceSets: readonly SourceSet[] = [
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
const missingDocs = (
  await Promise.all(sourceFiles.map(async (file: string): Promise<readonly MissingDoc[]> => checkFile(file)))
).flat();

if (missingDocs.length > 0) {
  for (const missing of missingDocs) {
    console.error(`${missing.file}:${missing.line}:${missing.column}: missing JSDoc for ${missing.declaration}`);
  }
  process.exitCode = 1;
}

/**
 * Resolves the frontend project root from either source or generated script paths.
 */
function frontendProjectRoot(currentScriptDir: string): string {
  const parentDir = dirname(currentScriptDir);
  if (basename(parentDir) === ".generated") {
    return resolve(parentDir, "..");
  }
  return resolve(currentScriptDir, "..");
}

/**
 * Collects source files from configured directories without adding a glob dependency.
 */
async function collectSourceFiles(sourceSetList: readonly SourceSet[]): Promise<readonly string[]> {
  const files: string[] = [];
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
async function checkFile(file: string): Promise<readonly MissingDoc[]> {
  const text = await readFile(file, "utf8");
  const sourceFile = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true);
  const diagnostics: MissingDoc[] = [];
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
function documentedDeclarationName(node: ts.Node): string | undefined {
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
function hasLeadingJsDoc(node: ts.Node, sourceFile: ts.SourceFile): boolean {
  const ranges = ts.getLeadingCommentRanges(sourceFile.text, node.getFullStart()) ?? [];
  return ranges.some((range: ts.CommentRange): boolean => {
    const comment = sourceFile.text.slice(range.pos, range.end).trimStart();
    return comment.startsWith("/**");
  });
}

/**
 * Converts a source node into a user-facing missing documentation diagnostic.
 */
function diagnostic(file: string, sourceFile: ts.SourceFile, node: ts.Node, declaration: string): MissingDoc {
  const position = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
  return {
    file,
    line: position.line + 1,
    column: position.character + 1,
    declaration,
  };
}
