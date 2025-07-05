===========================================
Automating tasks with Python and Boardswarm
===========================================

There are frequently times during development or when debugging an issue when
having the ability to automate frequently performed tasks or create a quick
script to loop testing a device that occasionaly fails can be really
beneficial. If the devices are already being used in boardswarm, doing this can
be trivial.

Here's a quick example using `Python <https://www.python.org/>`_ and
`pexpect <https://pexpect.readthedocs.io/en/stable/>`_, which powers up a
board, waits for a login prompt, logs in and powers the machine back off. (Not
particularly useful in this form, I know, but it does form the basis for a
number of tasks)::

    #!/usr/bin/python3
    import os
    import pexpect
    import time
    
    device="<your devices name here>"
    
    # Ensure device is off
    os.system(f'boardswarm-cli device {device} mode off')
    
    # Attach console
    child = pexpect.spawn(f'boardswarm-cli device {device} connect')
    
    # Power up board
    os.system(f'boardswarm-cli device {device} mode on')
    
    match = child.expect(["login:"], timeout=120)
    
    child.send("user\n")
    child.expect(["Password:"])
    
    child.send("user\n")
    time.sleep(4)
    child.expect(['$'])
    
    child.send("sudo poweroff\n")
    child.expect(["reboot: Power down"])

