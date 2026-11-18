def _colab_cli_drivefs_operation():
    import getpass
    import json
    import sys
    import types
    from IPython.display import display

    payload = json.loads(__COLAB_DRIVEFS_PAYLOAD__)
    request = payload["request"]
    result = dict(version=1, operation_id=request["operation_id"], mountpoint=request["mountpoint"],
                  ok=False, stage="error", error="remote_operation_failed", mounted=None, managed=None)
    credentials = None
    try:
        metadata = types.ModuleType("_colab_cli_drivefs_metadata")
        exec(compile(payload["shim_source"], "<colab-drivefs-metadata>", "exec"), metadata.__dict__)
        controller = types.ModuleType("_colab_cli_drivefs_controller")
        missing = object()
        previous = sys.modules.get("metadata_shim", missing)
        sys.modules["metadata_shim"] = metadata
        try:
            exec(compile(payload["controller_source"], "<colab-drivefs-controller>", "exec"), controller.__dict__)
        finally:
            if previous is missing:
                sys.modules.pop("metadata_shim", None)
            else:
                sys.modules["metadata_shim"] = previous
        if payload["action"] == "mount":
            credentials = json.loads(getpass.getpass(payload["prompt"]))
        result = controller.Controller().run(payload["action"], request, credentials, payload["shim_source"])
    except BaseException:
        pass  # Never print a traceback or JSON excerpt from the secret input.
    finally:
        credentials = None
    display({"application/vnd.colab-cli.drivefs-result+json": result}, raw=True)


_colab_cli_drivefs_operation()
del _colab_cli_drivefs_operation
