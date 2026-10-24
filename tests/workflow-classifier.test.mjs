import test from 'node:test';
import assert from 'node:assert/strict';
import {
  classifyWorkflowComplexity,
  executeWorkflow,
  formatWorkflowDetails,
  parseMarkdownWorkflow,
} from '../dist/workflow.js';

test('classifier: conservative default is single for single-action or ambiguous prompts', () => {
  const single = [
    'fix the login bug',
    'add a test for parseUser',
    'refactor src/utils.ts',
    'write docs for the CLI',
    'update package.json',
    'run the tests',
    'review the code and fix issues if needed', // one connector, two verbs -> ambiguous -> single
    'plan the feature', // keyword alone is not enough
    'chạy kiểm tra hồi quy', // Vietnamese verbs without sequencing stay single
    '',
  ];
  for (const prompt of single) {
    const result = classifyWorkflowComplexity(prompt);
    assert.equal(result.mode, 'single', `expected single for ${JSON.stringify(prompt)}: ${result.reason}`);
    assert.match(result.reason, /single:/);
  }
});

test('classifier: multi for three distinct imperative verbs', () => {
  const result = classifyWorkflowComplexity('Implement auth, fix the login bug and update the docs');
  assert.equal(result.mode, 'multi');
  assert.match(result.reason, /3 distinct imperative verbs/);
});

test('classifier: multi for two verbs chained by a sequencing connector', () => {
  for (const prompt of [
    'Fix the bug then update the tests',
    'Write the parser, after that update the docs',
    'thêm handler rồi... sau đó sửa test', // Vietnamese "sau đó"
  ]) {
    const result = classifyWorkflowComplexity(prompt);
    assert.equal(result.mode, 'multi', `expected multi for ${JSON.stringify(prompt)}: ${result.reason}`);
  }
});

test('classifier: multi for two sequencing connectors even with one verb kind', () => {
  const result = classifyWorkflowComplexity('run the suite, then run lint, then run build');
  assert.equal(result.mode, 'multi');
});

test('classifier: multi for multi-step keyword plus a second action signal', () => {
  for (const prompt of [
    'multi-step task: implement the parser and write tests',
    'plan the workflow then implement it',
  ]) {
    const result = classifyWorkflowComplexity(prompt);
    assert.equal(result.mode, 'multi', `expected multi for ${JSON.stringify(prompt)}: ${result.reason}`);
  }
});

test('classifier: multi for several path mentions plus multiple verbs', () => {
  const result = classifyWorkflowComplexity('fix and update src/a.ts, src/b.ts, src/c.ts');
  assert.equal(result.mode, 'multi');
  // One verb across three files stays single (conservative): a single action is not a workflow.
  assert.equal(classifyWorkflowComplexity('update src/a.ts, src/b.ts, src/c.ts').mode, 'single');
});

test('classifier: plain "and" never makes a task multi on its own', () => {
  const result = classifyWorkflowComplexity('Fix the bug and update the tests');
  assert.equal(result.mode, 'single');
});

test('classifier: caller-provided fileCount counts toward the path signal', () => {
  assert.equal(classifyWorkflowComplexity('update the handlers', { fileCount: 3 }).mode, 'single');
  const withFiles = classifyWorkflowComplexity('fix and update the handlers', { fileCount: 3 });
  assert.equal(withFiles.mode, 'multi');
});

test('workflow dag step: runs the DAG hook only when classifier says multi and multi-agent is enabled', async () => {
  const steps = [{ id: 'plan_and_build', title: 'DAG', type: 'dag', prompt: 'implement the parser, then fix the loader and update the tests' }];
  const calls = { dag: 0, prompt: 0, events: [] };
  const hooks = {
    cwd: '.',
    signal: AbortSignal.timeout(5000),
    runPrompt: async (prompt) => {
      calls.prompt++;
      return `prompt:${prompt.slice(0, 12)}`;
    },
    runDag: async (goal) => {
      calls.dag++;
      return `dag:${goal.slice(0, 12)}`;
    },
    multiAgentEnabled: true,
    onEvent: event => calls.events.push(event),
  };
  const res = await executeWorkflow({ name: 'w', description: 'd', scope: 'builtin', steps }, 'task input', hooks);
  assert.equal(res.status, 'completed');
  assert.equal(calls.dag, 1, 'the dag hook ran for a multi goal');
  assert.equal(calls.prompt, 0);
  assert.equal(calls.events.length, 1);
  assert.equal(calls.events[0].type, 'workflow_classified');
  assert.equal(calls.events[0].mode, 'multi');
  assert.match(calls.events[0].reason, /multi:/);
});

test('workflow dag step: degrades to a linear prompt when the classifier says single', async () => {
  const steps = [{ id: 's', title: 'DAG', type: 'dag', prompt: 'fix the login bug' }];
  const calls = { dag: 0, prompt: 0, events: [] };
  const res = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps },
    'input',
    {
      cwd: '.',
      signal: AbortSignal.timeout(5000),
      runPrompt: async (prompt) => {
        calls.prompt++;
        return `prompt:${prompt}`;
      },
      runDag: async () => {
        calls.dag++;
        return 'dag';
      },
      multiAgentEnabled: true,
      onEvent: event => calls.events.push(event),
    },
  );
  assert.equal(res.status, 'completed');
  assert.equal(calls.dag, 0, 'single goals never spawn a DAG');
  assert.equal(calls.prompt, 1);
  assert.equal(calls.events[0].mode, 'single');
  assert.match(res.finalOutput, /prompt:fix the login bug/);
});

test('workflow dag step: stays linear when multi-agent mode is off or the hook is missing', async () => {
  const steps = [{ id: 's', type: 'dag', prompt: 'implement the parser, then fix the loader and update the tests' }];
  const makeHooks = (extra = {}) => ({
    cwd: '.',
    signal: AbortSignal.timeout(5000),
    runPrompt: async () => 'linear',
    runDag: async () => {
      throw new Error('dag hook must not run');
    },
    ...extra,
  });
  const off = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps },
    'input',
    makeHooks({ multiAgentEnabled: false }),
  );
  assert.equal(off.finalOutput, 'linear', 'multiAgentEnabled=false keeps the workflow linear');

  const noHook = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps },
    'input',
    makeHooks({ multiAgentEnabled: true, runDag: undefined }),
  );
  assert.equal(noHook.finalOutput, 'linear', 'a missing runDag hook keeps the workflow linear');
});

test('old workflows without dag steps never classify and stay linear', async () => {
  const steps = [
    { id: 'one', type: 'prompt', prompt: 'implement the parser, then fix the loader and update the tests' },
    { id: 'two', type: 'prompt', prompt: 'done: $prev' },
  ];
  const events = [];
  const res = await executeWorkflow(
    { name: 'w', description: 'd', scope: 'builtin', steps },
    'input',
    {
      cwd: '.',
      signal: AbortSignal.timeout(5000),
      runPrompt: async prompt => prompt,
      multiAgentEnabled: true,
      onEvent: event => events.push(event),
    },
  );
  assert.equal(res.status, 'completed');
  assert.equal(events.length, 0, 'no workflow_classified event for legacy linear workflows');
  assert.match(res.finalOutput, /implement the parser/);
});

test('markdown workflows accept a [dag] step and formatWorkflowDetails renders it', () => {
  const wf = parseMarkdownWorkflow(
    'plan-build.md',
    '---\nname: plan-build\ndescription: plan then build\n---\n## Step 1: Decompose [dag]\nImplement the parser, then fix the loader and update the tests\n',
    'workspace',
    '/tmp/plan-build.md',
  );
  assert.equal(wf.steps.length, 1);
  assert.equal(wf.steps[0].type, 'dag');
  assert.match(wf.steps[0].prompt, /Implement the parser/);
  const details = formatWorkflowDetails(wf);
  assert.match(details, /dag goal="Implement the parser/);
});
