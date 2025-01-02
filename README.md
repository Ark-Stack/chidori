

## ArkStack - Build and Debug AI Agents with Ease;

**ArkStack** builds upon **[Chidori](https://github.com/ThousandBirdsInc/chidori)**, a reactive runtime for building durable AI agents. It extends Chidori’s capabilities by integrating **[smolagents](https://github.com/huggingface/smolagents)**, a lightweight library for code-driven AI agents, and adding features like **vector store integration**, **smolagents remote tools support**, and **enhanced debugging**. ArkStack provides a **feature rich frameowrk** for developers to build, and debug AI agents with ease.

For more details read the **[docs](https://github.com/Ark-Stack/docs)**

## **Features**

### **Reactive Runtime**
ArkStack leverages Chidori’s reactive runtime to orchestrate interactions between agents and their components. It supports **Python** and **JavaScript** code execution, enabling developers to build dynamic workflows with **pause/resume** and **time-travel debugging**.

### **Code-Driven Agents**
With **smolagents**, developers can create **code-driven agents** that generate and execute Python code, reducing the number of steps required to complete tasks and improving efficiency.

### **Vector Store Integration**
ArkStack supports **vector stores** for tasks like **retrieval-augmented generation (RAG)**, **semantic search**, and **memory management**. It includes a **local vector store** for development and integrates with external databases like **Pinecone** and **Weaviate**.

### **Remote Tools and Functions**
ArkStack provides a **marketplace and hub** for smolagents remote tools, enabling developers to dynamically discover, share, and integrate tools into agent workflows.

### **Enhanced Debugging and Observability**
ArkStack offers advanced debugging features, including **time-travel debugging**, **execution comparison**, and **interactive debugging**. Comprehensive **metrics dashboards** provide real-time insights into agent performance.

### **Multi-Agent Systems**
ArkStack supports **hierarchical multi-agent systems**, enabling agents to collaborate on complex tasks through **managed agents** and **communication protocols**.

### **Security and Sandboxing**
ArkStack ensures safe execution of generated code with **secure environments**, including the **local Python interpreter** and **E2B sandbox** (inherited from smolagents :D).

## **🛣️ Roadmap**

### Short term
* [x] Reactive subscriptions between nodes
* [x] Branching and time travel debugging, reverting execution of a graph
* [x] Node.js, Python, and Rust support for building and executing graphs
* [x] Simple local vector db for development
* [~] Integrate smolagents library for code generation execution (in-progress)
* [ ] Combine Chidori’s time-travel debugging with smolagents’ execution logs for a seamless debugging experience.
* [ ] Expand the tool registry to support dynamic tool discovery and registration.

### Medium term
* [x] Analysis tools for comparing executions
* [x] Adding support for more vector databases
* [x] Adding support for other LLM sources
* [x] Adding support for more code interpreter environments
* [ ] Port smolagents to TypeScript
* [ ] Launch tool marketplace
* [ ] Agent re-evaluation with feedback
* [ ] Add metrics, dashboards, and alerts for monitoring agent performance and behavior.
* [ ] Enable agents to dynamically adapt to changing contexts or environments during execution.

---

## **License**
ArkStack is under the MIT license. 

## **Help Us Out!**
Star the GitHub repo to stay updated and contribute!

## Inspiration
Our framework is inspired by the work of many others, including:
* [chidori](https://github.com/ThousandBirdsInc/chidori) - providing the reactive runtime
* [smolagents](https://github.com/huggingface/smolagents) - providing First-class support for Code Agents, i.e. agents that write their actions in code (as opposed to "agents being used to write code")
* [Temporal.io](https://temporal.io) - providing reliability and durability to workflows
* [Eve](http://witheve.com) - developing patterns for building reactive systems and reducing accidental complexity
* [Timely Dataflow](https://timelydataflow.github.io/timely-dataflow) - efficiently streaming changes
* [Langchain](https://www.langchain.com) - developing tools and patterns for building with LLMs
