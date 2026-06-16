declare module "monaco-editor/esm/vs/editor/editor.api.js" {
  export * from "monaco-editor";
}

declare module "*?worker" {
  const WorkerFactory: new () => Worker;
  export default WorkerFactory;
}

declare module "*?url" {
  const url: string;
  export default url;
}

declare global {
  var MonacoEnvironment:
    | {
        getWorker?: (workerId: string, label: string) => Worker;
      }
    | undefined;
}
