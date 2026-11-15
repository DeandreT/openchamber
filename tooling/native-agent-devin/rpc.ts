import { Rpc } from '@opencode/plugin/rpc';
import { nativeAgentRpcID } from '../../packages/sdk/src/native-agent.ts';
import { nativeAgentEventSchema, nativeAgentParametersSchemas, nativeAgentResultSchema } from '../../packages/sdk/src/native-agent-schemas.ts';

export const DevinNativeAgent = Rpc.define({
  id: nativeAgentRpcID('devin'),
  methods: {
    getState: { input: nativeAgentParametersSchemas.getState, output: nativeAgentResultSchema },
    prepare: { input: nativeAgentParametersSchemas.prepare, output: nativeAgentResultSchema },
    authenticate: { input: nativeAgentParametersSchemas.authenticate, output: nativeAgentResultSchema },
    setConfig: { input: nativeAgentParametersSchemas.setConfig, output: nativeAgentResultSchema },
    answer: { input: nativeAgentParametersSchemas.answer, output: nativeAgentResultSchema },
  },
  events: {
    stateChanged: { schema: nativeAgentEventSchema.options[0] },
    toolProgress: { schema: nativeAgentEventSchema.options[1] },
  },
});
