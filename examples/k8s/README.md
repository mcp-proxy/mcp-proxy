## Kubernetes Example

This example shows how to deploy the agentproxy in a Kubernetes cluster with a static config.

### Running the MCP Proxy


First let's apply our config to the cluster.
```bash
kubectl create -n agentproxy configmap agentproxy-config --from-file=config.json=examples/k8s/config.json
```

Now we can deploy the MCP Proxy.
```bash
kubectl apply -n agentproxy -f examples/k8s/manifest.yaml
```


### Deploying the example

```bash
kubectl apply -n agentproxy -f examples/k8s/manifest.yaml
```

Once all of the pods are up and running, you can test the proxy by port-forwarding the proxy pod, and then using the mcp inspector. In the first shell run:
```bash
kubectl port-forward -n agentproxy deploy/agentproxy 3000
```

In the second shell run:
```bash
npx npx @modelcontextprotocol/inspector
```

If everything worked correctly, you should be able to list tools and see the following:
![Inspector](./img/tools.png)

Let's try out one of the tools, like `everything:add`.

![Echo](./img/call.png)

To clean this up simply delete the namespace that this example ran in.

```bash
kubectl delete namespace agentproxy
```

