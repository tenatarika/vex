class Service {
  constructor(public name: string, private port: number) {
    this.name = name;
    this.port = port;
  }

  start(): void {
    console.log("starting");
  }
}
